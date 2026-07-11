//! `onebrain.yml` generation (v3.1+ canonical filename · renamed from
//! `vault.yml`).
//!
//! v3.4.8: the scaffold is a hand-authored, self-documenting template —
//! every key is preceded by a `# <what it is> · default: <value>` comment so
//! a user who misconfigures a value can always find its documented default in
//! the file itself (serde_yaml cannot emit comments, hence hand-authored).
//! The values are interpolated from the same `onebrain_core` default fns the
//! runtime falls back to, so template and runtime can never drift; a
//! round-trip test asserts the equality. The optional `schedule:` block is
//! still serialized from the user's chosen [`SchedulePreset`] via
//! `serde_yaml` (preset entries vary) and appended.
//!
//! v3.1 dual-read transition: this writer ALWAYS emits `onebrain.yml`. The
//! init runner (`super::run_init`) skips writing if either filename already
//! exists, so a legacy `vault.yml`-only vault is left untouched here —
//! `onebrain doctor --fix` performs the atomic rename instead.

use crate::error::FsError;
use crate::init::presets::{ScheduleEntry, SchedulePreset};
use crate::vault_sync::{DEFAULT_UPDATE_CHANNEL, VALID_UPDATE_CHANNELS};
use onebrain_core::config::{SearchConfig, TokenOptimizationConfig};
use onebrain_core::{CheckpointPolicy, RerankerConfig, VaultFolders, CONFIG_FILENAME};
use serde::Serialize;
use std::path::Path;

/// The `search.exclude` doc comment, shared by [`config_key_docs`] (generic
/// default, using `VaultFolders::default().archive`),
/// [`render_onebrain_yml_for_folders`] (the archive folder actually used for
/// that render), and doctor's `--fix` search-exclude backfill in
/// `onebrain-cli` (the existing vault's own resolved archive), so the
/// comment and the emitted value always agree — never a hard-coded
/// `"06-archive"` literal in any caller.
pub fn search_exclude_comment(archive: &str) -> String {
    format!(
        "# Extra index-exclusion patterns: vault-relative prefix or bare dir name \
         (hidden dirs and node_modules always skipped) · default: [attachments, {archive}]"
    )
}

/// Engine-calibrated reranker score gate, documented in the template. The
/// runtime source of truth is `onebrain_search::engine::DEFAULT_RERANK_MIN_SCORE`
/// — this crate cannot depend on `onebrain-search` (the init scaffold must
/// stay lightweight), so the value is mirrored here and pinned to the engine
/// constant by a cross-crate test in `onebrain-cli` (`init` command tests).
pub const TEMPLATE_RERANK_MIN_SCORE: &str = "0.30";

/// One template-known config key: its path segments and the exact
/// self-documentation comment line the fresh template writes directly above
/// it (leading `# ` included · indentation excluded).
///
/// Single source of truth shared by [`render_onebrain_yml`] (which
/// interpolates these very strings into the template) and doctor's `--fix`
/// comment-backfill recipe (which inserts them above uncommented keys in
/// EXISTING configs) — the two can never drift.
#[derive(Debug, Clone)]
pub struct ConfigKeyDoc {
    /// Key path, e.g. `["search", "reranker", "min_score"]`.
    pub segments: &'static [&'static str],
    /// The full comment line, e.g. `# Rerank stage on/off · default: true`.
    pub comment: String,
}

/// The self-documentation table: every key the template knows, with its
/// comment. Defaults are interpolated from the same `onebrain_core` default
/// fns the runtime uses.
pub fn config_key_docs() -> Vec<ConfigKeyDoc> {
    let channels = VALID_UPDATE_CHANNELS.join(" | ");
    let f = VaultFolders::default();
    let cp = CheckpointPolicy::default();
    let sc = SearchConfig::default();
    let rr = RerankerConfig::default();
    let eg = onebrain_core::config::EmbedGate::default();
    let tok = TokenOptimizationConfig::default();
    let doc =
        |segments: &'static [&'static str], comment: String| ConfigKeyDoc { segments, comment };
    vec![
        doc(
            &["update_channel"],
            format!(
                "# Release channel for plugin updates: {channels} · default: {DEFAULT_UPDATE_CHANNEL}"
            ),
        ),
        doc(
            &["folders", "inbox"],
            format!("# Raw braindumps and quick captures · default: {}", f.inbox),
        ),
        doc(
            &["folders", "projects"],
            format!(
                "# Active projects with tasks and notes · default: {}",
                f.projects
            ),
        ),
        doc(
            &["folders", "areas"],
            format!(
                "# Ongoing responsibilities (health, finances, ...) · default: {}",
                f.areas
            ),
        ),
        doc(
            &["folders", "knowledge"],
            format!(
                "# Your own synthesized thinking and insights · default: {}",
                f.knowledge
            ),
        ),
        doc(
            &["folders", "resources"],
            format!(
                "# External info: research output, summaries, reference · default: {}",
                f.resources
            ),
        ),
        doc(
            &["folders", "agent"],
            format!("# AI-specific context and memory · default: {}", f.agent),
        ),
        doc(
            &["folders", "archive"],
            format!(
                "# Completed projects and archived areas · default: {}",
                f.archive
            ),
        ),
        doc(
            &["folders", "logs"],
            format!(
                "# Session logs, checkpoints, and system logs · default: {}",
                f.logs
            ),
        ),
        doc(
            &["checkpoint", "messages"],
            format!(
                "# Message count between checkpoint emissions (>= 1) · default: {}",
                cp.messages
            ),
        ),
        doc(
            &["checkpoint", "minutes"],
            format!(
                "# Minutes between checkpoint emissions (>= 1) · default: {}",
                cp.minutes
            ),
        ),
        doc(
            &["search", "collection"],
            "# Collection name binding this vault to its index · default: unset".to_string(),
        ),
        doc(
            &["search", "embed_model"],
            format!(
                "# Embedding model, from `onebrain search model list` · default: {}",
                sc.embed_model
            ),
        ),
        doc(
            &["search", "default_top_k"],
            format!(
                "# Result count when a caller doesn't pass top_k (>= 1) · default: {}",
                sc.default_top_k
            ),
        ),
        doc(
            &["search", "reranker", "enabled"],
            format!("# Rerank stage on/off · default: {}", rr.enabled),
        ),
        doc(
            &["search", "reranker", "model"],
            format!(
                "# Reranker model, from `onebrain search model list` · default: {}",
                rr.model
            ),
        ),
        doc(
            &["search", "reranker", "min_candidates"],
            format!(
                "# Minimum candidate pool to rerank, a floor not a ceiling (>= 1) · default: {}",
                rr.min_candidates
            ),
        ),
        doc(
            &["search", "reranker", "min_score"],
            format!(
                "# Score gate: hits below this calibrated 0-1 score are dropped · default: {TEMPLATE_RERANK_MIN_SCORE}"
            ),
        ),
        // `search.exclude` IS emitted by the fresh template (v3.4.9+): the
        // template-purpose value is `[attachments, <archive>]`, resolved from
        // the render's `VaultFolders` — never the bare runtime serde default
        // (`default_search_exclude()` in onebrain-core stays `[attachments]`
        // only; that's a separate, deliberately unchanged spec decision for
        // existing vaults). The `search.embed.*` gate remains backfill-only
        // (parsed but not yet enforced) — same pattern as recap.*. Their docs
        // exist so `doctor --fix` backfills the comments on EXISTING vaults
        // that carry them. The completeness guard test keeps this table in
        // sync with the config structs.
        doc(&["search", "exclude"], search_exclude_comment(&f.archive)),
        doc(
            &["search", "embed", "auto"],
            format!("# Auto-embed changed docs on/off · default: {}", eg.auto),
        ),
        doc(
            &["search", "embed", "threshold"],
            format!(
                "# Changed docs required before an auto-embed run triggers (>= 1) · default: {}",
                eg.threshold
            ),
        ),
        doc(
            &["search", "embed", "debounce_seconds"],
            format!(
                "# Debounce window in seconds before an auto-embed run fires (>= 1) · default: {}",
                eg.debounce_seconds
            ),
        ),
        doc(
            &["search", "embed", "max_batch"],
            format!(
                "# Max docs embedded per batch (>= 1) · default: {}",
                eg.max_batch
            ),
        ),
        doc(
            &["search", "embed", "schedule"],
            "# Cron schedule for a periodic full re-embed · default: unset".to_string(),
        ),
        // Token-optimization ladder + cache + read-hook config (v3.4.10
        // design §5). The fresh template DOES emit this block (unlike
        // recap/stats/search.embed below) — token-opt is a headline feature,
        // documented in full in docs/token-optimization.md (Track 9).
        doc(
            &["token_optimization", "level"],
            format!(
                "# Optimization ladder rung: off | conservative | balanced | aggressive · default: {}",
                tok.level
            ),
        ),
        doc(
            &["token_optimization", "get_max_tokens"],
            format!(
                "# `get` continuation cap in estimated tokens, 0 = unlimited · default: {}",
                tok.get_max_tokens
            ),
        ),
        doc(
            &["token_optimization", "snippet_max_chars"],
            format!(
                "# Per-hit snippet length cap in characters · default: {}",
                tok.snippet_max_chars
            ),
        ),
        doc(
            &["token_optimization", "strip_frontmatter"],
            "# Strip YAML frontmatter from get/multi_get bodies: auto | always | never · default: auto"
                .to_string(),
        ),
        doc(
            &["token_optimization", "model"],
            format!(
                "# Model family hint for token estimation + pricing · default: {}",
                tok.model
            ),
        ),
        doc(
            &["token_optimization", "read_hook"],
            "# Vault-read ledger-gate hook: off | ledger · default: off".to_string(),
        ),
        // Plugin-level recap thresholds (`/recap` skill). Not part of
        // VaultConfig — the CLI never emits a recap block in the fresh
        // template (absent = the plugin uses its own defaults, so the two
        // can't drift). These docs exist so `doctor --fix` backfills the
        // comments for EXISTING vaults that already carry a recap block.
        // Defaults verified against the plugin source
        // (`skills/recap/SKILL.md`): min_sessions 6, min_frequency 2.
        doc(
            &["recap", "min_sessions"],
            "# Unrecapped session logs required before /recap runs (plugin) · default: 6"
                .to_string(),
        ),
        doc(
            &["recap", "min_frequency"],
            "# Sessions a topic must recur in to be promoted to memory (plugin) · default: 2"
                .to_string(),
        ),
        // System-managed doctor flag. Not part of the fresh template (a new
        // vault has never been prompted, so there's nothing to record yet) —
        // `doctor --fix` writes this key only after the user declines the
        // qmd leftover cleanup prompt, via the same comment-preserving line
        // edit `stamp_doctor_run` uses for `last_doctor_run`. The doc entry
        // exists so a manually-added or hand-edited `stats:` block still
        // gets the self-documentation comment backfilled.
        doc(
            &["stats", "qmd_cleanup_declined"],
            "# Set by doctor --fix after you decline the qmd cleanup prompt — suppresses re-prompting · default: unset".to_string(),
        ),
        // Block-level header for `schedule:` documenting the entry shape —
        // one multi-line comment (never per-entry comments; entry data is
        // never touched). Shared verbatim by the fresh template and the
        // `doctor --fix` backfill. Deliberately avoids the literal `cron:`
        // token so tests counting entries by that token stay accurate.
        doc(
            &["schedule"],
            "# Scheduled skill runs, compiled to the OS scheduler by `onebrain schedule register`.\n\
             # Each entry pairs a cron expression with either `skill: /name` (a OneBrain\n\
             # skill) or `command` + `args` (any CLI)."
                .to_string(),
        ),
    ]
}

/// Build the onebrain.yml text content for the given preset. Pure function —
/// useful for unit testing without touching the filesystem. Every per-key
/// comment line is interpolated from [`config_key_docs`], so template and
/// backfill share one definition.
pub fn render_onebrain_yml(preset: SchedulePreset) -> Result<String, FsError> {
    render_onebrain_yml_for_folders(preset, &VaultFolders::default())
}

/// Same as [`render_onebrain_yml`], parameterized by the vault's resolved
/// [`VaultFolders`] — the `search.exclude` block's archive entry is derived
/// from `folders.archive` at generation time rather than a hard-coded
/// literal. `render_onebrain_yml` is the production entry point (always
/// `VaultFolders::default()` — `onebrain init` does not yet let users
/// customize folder names); this parameterized variant exists so a test can
/// prove the archive value is resolved, not hard-coded.
fn render_onebrain_yml_for_folders(
    preset: SchedulePreset,
    f: &VaultFolders,
) -> Result<String, FsError> {
    let cp = CheckpointPolicy::default();
    let sc = SearchConfig::default();
    let rr = RerankerConfig::default();
    let tok = TokenOptimizationConfig::default();
    let docs = config_key_docs();
    let c = |segments: &[&str]| -> String {
        docs.iter()
            .find(|d| d.segments == segments)
            .expect("every template key has a doc entry")
            .comment
            .clone()
    };
    // File header (preamble): stays above the first section banner.
    let preamble: Vec<String> = "\
# onebrain.yml — vault configuration.
# Every key is annotated with what it does and its default value.
# `onebrain doctor` validates these values; `onebrain doctor --fix` resets
# out-of-range tunables to their defaults (folders.* and search.collection
# are reported only — never auto-reset)."
        .lines()
        .map(str::to_string)
        .collect();

    // Each top-level block is built here (lead comment + header + body) and
    // handed to the shared `config_layout::assemble`, which emits the section
    // banners and blank separators. Sharing the assembler with `doctor --fix`
    // guarantees the fresh template is already in canonical layout, so a
    // restructure of it is a byte-identical no-op (asserted by a round-trip
    // test).
    let lines_of = |s: String| -> Vec<String> { s.lines().map(str::to_string).collect() };
    let mut blocks: Vec<crate::config_layout::Block> = Vec::new();

    blocks.push(crate::config_layout::Block {
        key: "update_channel".to_string(),
        lines: lines_of(format!(
            "{}\nupdate_channel: {DEFAULT_UPDATE_CHANNEL}",
            c(&["update_channel"])
        )),
    });

    blocks.push(crate::config_layout::Block {
        key: "folders".to_string(),
        lines: lines_of(format!(
            "\
# Vault folder layout (PARA). Renaming a folder here does NOT move notes on
# disk — doctor reports problems but never rewrites these values.
folders:
  {c_inbox}
  inbox: {inbox}
  {c_projects}
  projects: {projects}
  {c_areas}
  areas: {areas}
  {c_knowledge}
  knowledge: {knowledge}
  {c_resources}
  resources: {resources}
  {c_agent}
  agent: {agent}
  {c_archive}
  archive: {archive}
  {c_logs}
  logs: {logs}",
            c_inbox = c(&["folders", "inbox"]),
            c_projects = c(&["folders", "projects"]),
            c_areas = c(&["folders", "areas"]),
            c_knowledge = c(&["folders", "knowledge"]),
            c_resources = c(&["folders", "resources"]),
            c_agent = c(&["folders", "agent"]),
            c_archive = c(&["folders", "archive"]),
            c_logs = c(&["folders", "logs"]),
            inbox = f.inbox,
            projects = f.projects,
            areas = f.areas,
            knowledge = f.knowledge,
            resources = f.resources,
            agent = f.agent,
            archive = f.archive,
            logs = f.logs,
        )),
    });

    blocks.push(crate::config_layout::Block {
        key: "checkpoint".to_string(),
        lines: lines_of(format!(
            "\
# Stop-hook checkpoint thresholds (when to snapshot session context).
checkpoint:
  {c_messages}
  messages: {messages}
  {c_minutes}
  minutes: {minutes}",
            c_messages = c(&["checkpoint", "messages"]),
            c_minutes = c(&["checkpoint", "minutes"]),
            messages = cp.messages,
            minutes = cp.minutes,
        )),
    });

    blocks.push(crate::config_layout::Block {
        key: "search".to_string(),
        lines: lines_of(format!(
            "\
# Native search (BM25 + vector + Tier-2 reranker).
search:
  {c_collection}
  # (leave commented to keep search disabled — `onebrain search reindex` sets it)
  # collection: <set by onebrain search reindex>
  {c_embed_model}
  embed_model: {embed_model}
  {c_default_top_k}
  default_top_k: {default_top_k}
  {c_exclude}
  exclude:
  - attachments
  - {archive}
  # Tier-2 cross-encoder reranker (re-scores fused candidates for relevance).
  reranker:
    {c_enabled}
    enabled: {enabled}
    {c_model}
    model: {model}
    {c_min_candidates}
    min_candidates: {min_candidates}
    {c_min_score}
    min_score: {min_score}",
            c_collection = c(&["search", "collection"]),
            c_embed_model = c(&["search", "embed_model"]),
            c_default_top_k = c(&["search", "default_top_k"]),
            c_exclude = search_exclude_comment(&f.archive),
            archive = f.archive,
            c_enabled = c(&["search", "reranker", "enabled"]),
            c_model = c(&["search", "reranker", "model"]),
            c_min_candidates = c(&["search", "reranker", "min_candidates"]),
            c_min_score = c(&["search", "reranker", "min_score"]),
            embed_model = sc.embed_model,
            default_top_k = sc.default_top_k,
            enabled = rr.enabled,
            model = rr.model,
            min_candidates = rr.min_candidates,
            min_score = TEMPLATE_RERANK_MIN_SCORE,
        )),
    });

    blocks.push(crate::config_layout::Block {
        key: "token_optimization".to_string(),
        lines: lines_of(format!(
            "\
# Token optimization ladder + cache + read-hook (see docs/token-optimization.md).
token_optimization:
  {c_level}
  level: {level}
  {c_get_max_tokens}
  get_max_tokens: {get_max_tokens}
  {c_snippet_max_chars}
  snippet_max_chars: {snippet_max_chars}
  {c_strip_frontmatter}
  strip_frontmatter: {strip_frontmatter}
  {c_model}
  model: {model}
  {c_read_hook}
  read_hook: {read_hook}",
            c_level = c(&["token_optimization", "level"]),
            c_get_max_tokens = c(&["token_optimization", "get_max_tokens"]),
            c_snippet_max_chars = c(&["token_optimization", "snippet_max_chars"]),
            c_strip_frontmatter = c(&["token_optimization", "strip_frontmatter"]),
            c_model = c(&["token_optimization", "model"]),
            c_read_hook = c(&["token_optimization", "read_hook"]),
            level = tok.level,
            get_max_tokens = tok.get_max_tokens,
            snippet_max_chars = tok.snippet_max_chars,
            strip_frontmatter = tok.strip_frontmatter,
            model = tok.model,
            read_hook = tok.read_hook,
        )),
    });

    let entries = preset.entries();
    if !entries.is_empty() {
        // The schedule block is emitted only when the preset carries entries,
        // so the Skip preset's file contains no `schedule` mention at all.
        #[derive(Serialize)]
        struct ScheduleBlock {
            schedule: Vec<ScheduleEntry>,
        }
        let serialized =
            serde_yaml::to_string(&ScheduleBlock { schedule: entries }).map_err(|e| {
                FsError::Io {
                    path: std::path::PathBuf::from(CONFIG_FILENAME),
                    source: std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()),
                }
            })?;
        let mut lines = lines_of(c(&["schedule"]));
        lines.extend(serialized.trim_end().lines().map(str::to_string));
        blocks.push(crate::config_layout::Block {
            key: "schedule".to_string(),
            lines,
        });
    }

    Ok(crate::config_layout::assemble(
        &preamble, &blocks, "\n", true,
    ))
}

/// Write `onebrain.yml` into `vault_dir` with the chosen preset's schedule
/// entries. Overwrites unconditionally — the guard belongs in
/// [`super::run_init`].
pub fn write_onebrain_yml(vault_dir: &Path, preset: SchedulePreset) -> Result<(), FsError> {
    let path = vault_dir.join(CONFIG_FILENAME);
    let content = render_onebrain_yml(preset)?;
    std::fs::write(&path, content).map_err(|e| FsError::Io {
        path: path.clone(),
        source: e,
    })
}

/// Convenience: parse an onebrain.yml string back into a dynamic `serde_yaml::Value`
/// (used in tests to assert round-trip integrity).
#[allow(dead_code)]
pub(crate) fn parse_onebrain_yml(content: &str) -> Result<serde_yaml::Value, serde_yaml::Error> {
    serde_yaml::from_str(content)
}

#[cfg(test)]
#[allow(unused_imports)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn template_has_a_comment_for_every_key() {
        let yaml = render_onebrain_yml(SchedulePreset::Skip).unwrap();
        for key in [
            "update_channel",
            "inbox",
            "projects",
            "areas",
            "knowledge",
            "resources",
            "agent",
            "archive",
            "logs",
            "messages",
            "minutes",
            "default_top_k",
            "embed_model",
            "enabled",
            "model",
            "min_candidates",
            "min_score",
            "collection",
            "level",
            "get_max_tokens",
            "snippet_max_chars",
            "strip_frontmatter",
            "read_hook",
        ] {
            // The line ABOVE each key line must be a `# … · default: …`
            // comment — walk the lines and assert the pairing, not just
            // substring presence. `collection` is allowed to be a commented
            // placeholder (`# collection:`).
            let idx = yaml
                .lines()
                .position(|l| {
                    l.trim_start().starts_with(&format!("{key}:"))
                        || l.trim_start().starts_with(&format!("# {key}:"))
                })
                .unwrap_or_else(|| panic!("missing key {key}:\n{yaml}"));
            assert!(
                yaml.lines()
                    .nth(idx.saturating_sub(1))
                    .unwrap()
                    .trim_start()
                    .starts_with('#'),
                "no doc comment above {key}:\n{yaml}"
            );
        }
        assert!(
            yaml.contains("· default:"),
            "comments must state defaults:\n{yaml}"
        );
        // `collection` stays a COMMENTED placeholder — absent = search
        // disabled semantics preserved (reindex/model-set activates it).
        assert!(yaml.contains("# collection:"), "{yaml}");
        let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        assert!(parsed.get("collection").is_none());
        assert!(parsed["search"].get("collection").is_none());
        // Search block is ACTIVE with real defaults.
        assert_eq!(parsed["search"]["default_top_k"].as_u64(), Some(10));
        assert_eq!(
            parsed["search"]["reranker"]["model"].as_str(),
            Some("onebrain-rerank-v1")
        );
    }

    #[test]
    fn template_values_match_runtime_defaults() {
        // Guard against drift: the values the template scaffolds must equal
        // the serde default fns the runtime falls back to when a key is
        // absent — sourced from onebrain-core, not re-invented literals.
        use onebrain_core::config::SearchConfig;
        use onebrain_core::{CheckpointPolicy, RerankerConfig, VaultFolders};

        let yaml = render_onebrain_yml(SchedulePreset::Skip).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();

        let cp = CheckpointPolicy::default();
        assert_eq!(
            parsed["checkpoint"]["messages"].as_u64(),
            Some(cp.messages as u64)
        );
        assert_eq!(
            parsed["checkpoint"]["minutes"].as_u64(),
            Some(cp.minutes as u64)
        );

        let f = VaultFolders::default();
        for (key, expect) in [
            ("inbox", &f.inbox),
            ("projects", &f.projects),
            ("areas", &f.areas),
            ("knowledge", &f.knowledge),
            ("resources", &f.resources),
            ("agent", &f.agent),
            ("archive", &f.archive),
            ("logs", &f.logs),
        ] {
            assert_eq!(
                parsed["folders"][key].as_str(),
                Some(expect.as_str()),
                "folders.{key} drifted from VaultFolders::default()"
            );
        }

        let sc = SearchConfig::default();
        assert_eq!(
            parsed["search"]["embed_model"].as_str(),
            Some(sc.embed_model.as_str())
        );
        assert_eq!(
            parsed["search"]["default_top_k"].as_u64(),
            Some(sc.default_top_k as u64)
        );

        let rr = RerankerConfig::default();
        assert_eq!(
            parsed["search"]["reranker"]["enabled"].as_bool(),
            Some(rr.enabled)
        );
        assert_eq!(
            parsed["search"]["reranker"]["model"].as_str(),
            Some(rr.model.as_str())
        );
        assert_eq!(
            parsed["search"]["reranker"]["min_candidates"].as_u64(),
            Some(rr.min_candidates as u64)
        );

        assert_eq!(
            parsed.get("update_channel").and_then(|v| v.as_str()),
            Some(crate::vault_sync::DEFAULT_UPDATE_CHANNEL)
        );

        let tok = TokenOptimizationConfig::default();
        assert_eq!(
            parsed["token_optimization"]["level"].as_str(),
            Some(tok.level.to_string()).as_deref()
        );
        assert_eq!(
            parsed["token_optimization"]["get_max_tokens"].as_u64(),
            Some(tok.get_max_tokens as u64)
        );
        assert_eq!(
            parsed["token_optimization"]["snippet_max_chars"].as_u64(),
            Some(tok.snippet_max_chars as u64)
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
    }

    #[test]
    fn template_comment_above_each_key_is_exactly_the_doc_table_entry() {
        // The backfill recipe inserts `config_key_docs` comments into
        // existing configs; the template interpolates the same strings. This
        // pins the pairing per key: the line(s) directly above each ACTIVE
        // key are exactly the table's comment (indentation aside; multi-line
        // comments like the schedule header are compared line by line).
        // `collection` is a commented placeholder — its doc comment appears
        // above the placeholder block instead. Essentials preset so the
        // `schedule` block (and its header doc) is present.
        let yaml = render_onebrain_yml(SchedulePreset::Essentials).unwrap();
        let lines: Vec<&str> = yaml.lines().collect();
        // Two leaf keys share a bare name across different blocks
        // (`search.reranker.model` and `token_optimization.model`) — track how
        // many times each literal key text has already been matched so the
        // Nth occurrence in the template pairs with the Nth entry in
        // `config_key_docs()` (both walk in the same top-to-bottom order).
        let mut occurrence_seen: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();
        for doc in config_key_docs() {
            let key = doc.segments.last().unwrap();
            if doc.segments == ["search", "collection"] {
                assert!(
                    yaml.contains(&format!("  {}\n", doc.comment)),
                    "collection doc comment missing:\n{yaml}"
                );
                continue;
            }
            // Backfill-only entries: their docs exist only for the doctor
            // --fix backfill on existing vaults; the fresh template never
            // emits these keys (recap.* = plugin-level, absent = plugin
            // defaults; stats.* = doctor-managed, absent until the first
            // `--fix` decline; search.embed.* = parsed but not yet enforced).
            // search.exclude is NOT backfill-only — the fresh template emits
            // it (see `render_onebrain_yml_for_folders`); it falls through
            // to the normal assertion below like any other emitted key.
            if doc.segments.first() == Some(&"recap") {
                assert!(
                    !yaml.contains("recap:"),
                    "fresh template must not emit a recap block:\n{yaml}"
                );
                continue;
            }
            if doc.segments.first() == Some(&"stats") {
                assert!(
                    !yaml.contains("stats:"),
                    "fresh template must not emit a stats block:\n{yaml}"
                );
                continue;
            }
            if doc.segments.get(1) == Some(&"embed") {
                assert!(
                    !yaml.contains("\n  embed:"),
                    "fresh template must not emit an embed block:\n{yaml}"
                );
                continue;
            }
            let skip = *occurrence_seen.get(*key).unwrap_or(&0);
            let idx = lines
                .iter()
                .enumerate()
                .filter(|(_, l)| l.trim_start().starts_with(&format!("{key}:")))
                .nth(skip)
                .map(|(i, _)| i)
                .unwrap_or_else(|| panic!("key {key} (occurrence {skip}) not in template"));
            occurrence_seen.insert(*key, skip + 1);
            let doc_lines: Vec<&str> = doc.comment.split('\n').collect();
            let above: Vec<&str> = lines[idx - doc_lines.len()..idx]
                .iter()
                .map(|l| l.trim_start())
                .collect();
            assert_eq!(
                above, doc_lines,
                "comment above {key} drifted from config_key_docs"
            );
        }
    }

    #[test]
    fn every_config_struct_key_has_a_doc_entry() {
        // Completeness guard (maintainer standing requirement): every real
        // user-facing config key must have a comment + default in
        // `config_key_docs` — the table drives BOTH the init template and
        // the doctor --fix backfill, so a key missing here ships
        // undocumented on every surface. Enumerate the key paths from the
        // serde config structs themselves (fully populated: every Option
        // Some, every Vec non-empty) so adding a struct field without a doc
        // entry fails THIS test with instructions.
        //
        // `stats.last_doctor_run` / `stats.last_doctor_fix` never appear in
        // this walk: they are not `VaultStats` fields — doctor writes them as
        // raw text (system-managed timestamps), so no allowlist entry is
        // needed for either. `stats.qmd_cleanup_declined` IS a real
        // `VaultStats` field (read back to gate the `doctor --fix` re-prompt),
        // so it walks like any other key and needs its own doc entry below.
        use onebrain_core::config::{EmbedGate, TokenOptimizationConfig};
        use onebrain_core::{VaultConfig, VaultStats};
        let cfg = VaultConfig {
            qmd_collection: Some("legacy".to_string()),
            checkpoint: CheckpointPolicy::default(),
            folders: VaultFolders::default(),
            search: SearchConfig {
                collection: Some("c".to_string()),
                embed: EmbedGate {
                    schedule: Some("0 3 * * 0".to_string()),
                    ..EmbedGate::default()
                },
                reranker: RerankerConfig {
                    min_score: Some(0.30),
                    ..RerankerConfig::default()
                },
                ..SearchConfig::default()
            },
            // Every field is a concrete scalar/enum (no `Option`), so the
            // plain default already exercises every leaf path the walk
            // below needs to see.
            token_optimization: TokenOptimizationConfig::default(),
            stats: VaultStats {
                qmd_cleanup_declined: Some(true),
            },
        };
        fn walk(v: &serde_yaml::Value, prefix: &[String], out: &mut Vec<String>) {
            match v.as_mapping() {
                Some(m) => {
                    for (k, child) in m {
                        let mut p = prefix.to_vec();
                        p.push(k.as_str().expect("string config key").to_string());
                        walk(child, &p, out);
                    }
                }
                None => out.push(prefix.join(".")),
            }
        }
        let value = serde_yaml::to_value(&cfg).unwrap();
        let mut paths: Vec<String> = Vec::new();
        walk(&value, &[], &mut paths);
        assert!(
            paths.len() >= 20,
            "walk must see the full config: {paths:?}"
        );

        // Explicit allowlist — every entry carries its written reason.
        let allowlist: &[(&str, &str)] = &[(
            "qmd_collection",
            "legacy v2 key, deprecated: replaced by search.collection; \
             doctor --fix migrates it away, so it must never be documented \
             as a current key",
        )];
        let documented: Vec<String> = config_key_docs()
            .iter()
            .map(|d| d.segments.join("."))
            .collect();
        for path in &paths {
            if allowlist.iter().any(|(k, _)| k == path) {
                continue;
            }
            assert!(
                documented.contains(path),
                "new config key `{path}` has no config_key_docs entry — add its \
                 comment/default/section to the table in onebrain_yml.rs (this \
                 drives the init template AND doctor --fix backfill)"
            );
        }
    }

    #[test]
    fn fresh_template_carries_section_banners() {
        let yaml = render_onebrain_yml(SchedulePreset::Essentials).unwrap();
        for title in [
            "General",
            "Vault layout",
            "Agent behavior",
            "Search",
            "Token optimization",
            "Automation",
        ] {
            assert!(
                yaml.contains(&crate::config_layout::section_banner(title)),
                "missing {title} banner:\n{yaml}"
            );
        }
        // Sections present in the template but not System (no stats scaffolded).
        assert!(!yaml.contains(&crate::config_layout::section_banner("System")));
        // Schedule entry-shape documentation is present.
        assert!(yaml.contains("`command` + `args` (any CLI)"));
    }

    #[test]
    fn restructure_of_fresh_template_is_a_noop() {
        // The fresh template must already be in canonical section layout, so
        // `doctor --fix`'s restructure never touches a freshly init'd vault
        // (idempotency + zero drift). Guards render↔restructure agreement.
        for preset in [
            SchedulePreset::Skip,
            SchedulePreset::Minimal,
            SchedulePreset::Essentials,
            SchedulePreset::MaintenancePlus,
        ] {
            let yaml = render_onebrain_yml(preset).unwrap();
            assert!(
                crate::config_layout::config_layout_matches(&yaml),
                "fresh template ({preset:?}) drifts from canonical layout:\n{yaml}"
            );
            assert_eq!(
                crate::config_layout::restructure_config(&yaml).as_deref(),
                Some(yaml.as_str()),
                "restructure of fresh template ({preset:?}) is not byte-identical"
            );
        }
    }

    #[test]
    fn fresh_template_excludes_configured_archive_folder() {
        // The exclude block's second entry must be resolved from the
        // vault's `folders.archive` at generation time, never a hard-coded
        // "06-archive" literal — this proves it by rendering with a
        // deliberately non-default archive folder name.
        fn render_fresh_template_with_archive(archive: &str) -> String {
            let folders = VaultFolders {
                archive: archive.to_string(),
                ..VaultFolders::default()
            };
            render_onebrain_yml_for_folders(SchedulePreset::Skip, &folders).unwrap()
        }
        let yaml = render_fresh_template_with_archive("z-archive");
        assert!(yaml.contains("\n  exclude:\n  - attachments\n  - z-archive\n"));
        // Scoped to the exclude block itself (not a blanket check over the
        // whole template): the pre-existing `folders.archive` doc comment
        // documents the tool's generic DEFAULT ("06-archive"), same as every
        // other folders.* comment — that's unrelated, unchanged behavior.
        // What must never happen is the exclude block falling back to the
        // hard-coded default instead of the resolved archive folder.
        assert!(!yaml.contains("- 06-archive"));
        assert!(!yaml.contains("attachments, 06-archive"));
    }

    #[test]
    fn render_skip_preset_omits_schedule_key() {
        let yaml = render_onebrain_yml(SchedulePreset::Skip).unwrap();
        assert!(
            !yaml.contains("schedule"),
            "Skip preset should not write a schedule: key, got:\n{yaml}"
        );
        assert!(yaml.contains("update_channel: stable"));
    }

    #[test]
    fn render_essentials_preset_writes_three_entries() {
        let yaml = render_onebrain_yml(SchedulePreset::Essentials).unwrap();
        assert!(yaml.contains("schedule:"));
        // /daily, /weekly, /recap
        assert!(yaml.contains("/daily"));
        assert!(yaml.contains("/weekly"));
        assert!(yaml.contains("/recap"));
    }

    #[test]
    fn round_trip_preserves_update_channel_and_folders() {
        let yaml = render_onebrain_yml(SchedulePreset::Essentials).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(
            parsed.get("update_channel").and_then(|v| v.as_str()),
            Some("stable")
        );
        let folders = parsed.get("folders").unwrap();
        assert_eq!(
            folders.get("inbox").and_then(|v| v.as_str()),
            Some("00-inbox")
        );
        assert_eq!(
            folders.get("logs").and_then(|v| v.as_str()),
            Some("07-logs")
        );
    }

    #[test]
    fn round_trip_preserves_checkpoint_defaults() {
        let yaml = render_onebrain_yml(SchedulePreset::Minimal).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        let cp = parsed.get("checkpoint").unwrap();
        assert_eq!(cp.get("messages").and_then(|v| v.as_u64()), Some(15));
        assert_eq!(cp.get("minutes").and_then(|v| v.as_u64()), Some(30));
    }

    #[test]
    fn write_creates_onebrain_yml_on_disk() {
        let d = tempdir().unwrap();
        write_onebrain_yml(d.path(), SchedulePreset::Skip).unwrap();
        // Canonical filename — NOT vault.yml.
        assert!(d.path().join("onebrain.yml").is_file());
        assert!(!d.path().join("vault.yml").exists());
        let content = std::fs::read_to_string(d.path().join("onebrain.yml")).unwrap();
        // v3.4.8: the file opens with the self-documentation header comment.
        assert!(content.starts_with("# onebrain.yml"));
        assert!(content.contains("\nupdate_channel: stable\n"));
    }

    #[test]
    fn maintenance_plus_writes_six_entries() {
        let yaml = render_onebrain_yml(SchedulePreset::MaintenancePlus).unwrap();
        // Count `cron:` occurrences — each entry has one.
        let n = yaml.matches("cron:").count();
        assert_eq!(n, 6, "Maintenance Plus should write 6 schedule entries");
        assert!(yaml.contains("command: onebrain"));
        assert!(yaml.contains("qmd-reindex"));
    }

    #[test]
    fn unused_indexmap_import_used_via_presets() {
        // Defensive: ScheduleEntry::Skill uses IndexMap. Linker check.
        let _ = indexmap::IndexMap::<String, String>::new();
    }
}
