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
use onebrain_core::config::SearchConfig;
use onebrain_core::{CheckpointPolicy, RerankerConfig, VaultFolders, CONFIG_FILENAME};
use serde::Serialize;
use std::path::Path;

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
    ]
}

/// Build the onebrain.yml text content for the given preset. Pure function —
/// useful for unit testing without touching the filesystem. Every per-key
/// comment line is interpolated from [`config_key_docs`], so template and
/// backfill share one definition.
pub fn render_onebrain_yml(preset: SchedulePreset) -> Result<String, FsError> {
    let f = VaultFolders::default();
    let cp = CheckpointPolicy::default();
    let sc = SearchConfig::default();
    let rr = RerankerConfig::default();
    let docs = config_key_docs();
    let c = |segments: &[&str]| -> String {
        docs.iter()
            .find(|d| d.segments == segments)
            .expect("every template key has a doc entry")
            .comment
            .clone()
    };
    let mut out = format!(
        "\
# onebrain.yml — vault configuration.
# Every key is annotated with what it does and its default value.
# `onebrain doctor` validates these values; `onebrain doctor --fix` resets
# out-of-range tunables to their defaults (folders.* and search.collection
# are reported only — never auto-reset).

{c_update_channel}
update_channel: {DEFAULT_UPDATE_CHANNEL}

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
  logs: {logs}

# Stop-hook checkpoint thresholds (when to snapshot session context).
checkpoint:
  {c_messages}
  messages: {messages}
  {c_minutes}
  minutes: {minutes}

# Native search (BM25 + vector + Tier-2 reranker).
search:
  {c_collection}
  # (leave commented to keep search disabled — `onebrain search reindex` sets it)
  # collection: <set by onebrain search reindex>
  {c_embed_model}
  embed_model: {embed_model}
  {c_default_top_k}
  default_top_k: {default_top_k}
  # Tier-2 cross-encoder reranker (re-scores fused candidates for relevance).
  reranker:
    {c_enabled}
    enabled: {enabled}
    {c_model}
    model: {model}
    {c_min_candidates}
    min_candidates: {min_candidates}
    {c_min_score}
    min_score: {min_score}
",
        c_update_channel = c(&["update_channel"]),
        c_inbox = c(&["folders", "inbox"]),
        c_projects = c(&["folders", "projects"]),
        c_areas = c(&["folders", "areas"]),
        c_knowledge = c(&["folders", "knowledge"]),
        c_resources = c(&["folders", "resources"]),
        c_agent = c(&["folders", "agent"]),
        c_archive = c(&["folders", "archive"]),
        c_logs = c(&["folders", "logs"]),
        c_messages = c(&["checkpoint", "messages"]),
        c_minutes = c(&["checkpoint", "minutes"]),
        c_collection = c(&["search", "collection"]),
        c_embed_model = c(&["search", "embed_model"]),
        c_default_top_k = c(&["search", "default_top_k"]),
        c_enabled = c(&["search", "reranker", "enabled"]),
        c_model = c(&["search", "reranker", "model"]),
        c_min_candidates = c(&["search", "reranker", "min_candidates"]),
        c_min_score = c(&["search", "reranker", "min_score"]),
        inbox = f.inbox,
        projects = f.projects,
        areas = f.areas,
        knowledge = f.knowledge,
        resources = f.resources,
        agent = f.agent,
        archive = f.archive,
        logs = f.logs,
        messages = cp.messages,
        minutes = cp.minutes,
        embed_model = sc.embed_model,
        default_top_k = sc.default_top_k,
        enabled = rr.enabled,
        model = rr.model,
        min_candidates = rr.min_candidates,
        min_score = TEMPLATE_RERANK_MIN_SCORE,
    );

    let entries = preset.entries();
    if !entries.is_empty() {
        // The block header comment is appended only alongside the entries so
        // the Skip preset's file contains no `schedule` mention at all.
        #[derive(Serialize)]
        struct ScheduleBlock {
            schedule: Vec<ScheduleEntry>,
        }
        let block = serde_yaml::to_string(&ScheduleBlock { schedule: entries }).map_err(|e| {
            FsError::Io {
                path: std::path::PathBuf::from(CONFIG_FILENAME),
                source: std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()),
            }
        })?;
        out.push_str("\n# Scheduled skill runs, compiled to the OS scheduler by `onebrain schedule register`.\n");
        out.push_str(&block);
    }
    Ok(out)
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
    }

    #[test]
    fn template_comment_above_each_key_is_exactly_the_doc_table_entry() {
        // The backfill recipe inserts `config_key_docs` comments into
        // existing configs; the template interpolates the same strings. This
        // pins the pairing per key: the line directly above each ACTIVE key
        // is exactly the table's comment (indentation aside). `collection`
        // is a commented placeholder — its doc comment appears above the
        // placeholder block instead.
        let yaml = render_onebrain_yml(SchedulePreset::Skip).unwrap();
        let lines: Vec<&str> = yaml.lines().collect();
        for doc in config_key_docs() {
            let key = doc.segments.last().unwrap();
            if doc.segments == ["search", "collection"] {
                assert!(
                    yaml.contains(&format!("  {}\n", doc.comment)),
                    "collection doc comment missing:\n{yaml}"
                );
                continue;
            }
            let idx = lines
                .iter()
                .position(|l| l.trim_start().starts_with(&format!("{key}:")))
                .unwrap_or_else(|| panic!("key {key} not in template"));
            assert_eq!(
                lines[idx - 1].trim_start(),
                doc.comment,
                "comment above {key} drifted from config_key_docs"
            );
        }
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
