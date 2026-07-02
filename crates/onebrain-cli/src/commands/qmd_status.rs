//! `onebrain qmd status` — report qmd index + embedding health.
//!
//! Vault-required (exit 64 outside a vault). Reads `qmd_collection` from
//! `onebrain.yml`, then shells out to `qmd status` and presents the parsed
//! figures in the requested output mode. Text is the default; `--json` /
//! `--yaml` emit the structured report for machine consumers.

use crate::legacy_output::serialize_for_mode;
use crate::output::{item, section, OutputMode};
use crate::vault_ctx;
use anyhow::{Context, Result};
use onebrain_cache::{query_status, QmdStatus};
use onebrain_core::load_vault_config;
use serde::Serialize;
use std::path::PathBuf;

/// Combined report: the configured collection (from `onebrain.yml`) plus the
/// figures qmd reports. `qmd_available` is `false` when the qmd binary is
/// missing or unresponsive — the index fields are then all `null`.
#[derive(Debug, Serialize)]
struct QmdStatusReport {
    /// `qmd_collection` from `onebrain.yml`, or `null` when unset.
    collection: Option<String>,
    /// Whether the `qmd` binary responded.
    qmd_available: bool,
    #[serde(flatten)]
    index: QmdStatus,
}

impl QmdStatusReport {
    /// Build a report from the configured collection and the result of
    /// [`query_status`]. The single construction site that ties
    /// `qmd_available` to index presence: `None` (qmd absent/unresponsive) ⟹
    /// `qmd_available: false` + a default (all-`None`) index, so the
    /// "available but populated" / "unavailable but populated" contradictions
    /// can't be built.
    fn from_query(collection: Option<String>, qmd: Option<QmdStatus>) -> Self {
        Self {
            collection,
            qmd_available: qmd.is_some(),
            index: qmd.unwrap_or_default(),
        }
    }
}

pub fn run(vault_flag: Option<PathBuf>, mode: &OutputMode) -> Result<()> {
    // Vault-required: exit 64 (E_VAULT_NOT_FOUND) outside a vault.
    let resolved = vault_ctx::require(vault_flag)?;
    let config = load_vault_config(&resolved.root).context("load vault config")?;

    let report = QmdStatusReport::from_query(config.qmd_collection, query_status());

    println!("{}", format_output(&report, mode));
    Ok(())
}

/// Render `report` for the requested output mode. Text is human-facing;
/// json/yaml go through the shared serializer.
fn format_output(report: &QmdStatusReport, mode: &OutputMode) -> String {
    if let OutputMode::Text { .. } = mode {
        render_text(report)
    } else {
        serialize_for_mode(report, mode)
    }
}

fn render_text(report: &QmdStatusReport) -> String {
    // Grouped-status convention (matches `search status`): a `🔍  Qmd index`
    // section header, then indented fixed-width label rows, then an optional
    // hint. Legacy qmd surface — kept in sync with the house style until qmd is
    // removed in v3.4.2.
    let mut lines = vec![section("🔍", "Qmd index")];

    match &report.collection {
        Some(c) => lines.push(item("Collection", c)),
        None => lines.push(item(
            "Collection",
            "not set — run /qmd to configure qmd_collection in onebrain.yml",
        )),
    }

    if !report.qmd_available {
        lines.push(item("Qmd", "not installed or not responding"));
        return lines.join("\n");
    }

    let idx = &report.index;
    let num = |n: Option<u64>| n.map(|v| v.to_string()).unwrap_or_else(|| "?".to_string());
    lines.push(item(
        "Documents",
        &format!(
            "{} indexed · {} embedded · {} pending",
            num(idx.total_files),
            num(idx.embedded_vectors),
            num(idx.pending_embedding),
        ),
    ));
    if let Some(size) = &idx.index_size {
        lines.push(item("Index size", size));
    }
    if let Some(updated) = &idx.last_updated {
        lines.push(item("Updated", updated));
    }
    if let Some(pending) = idx.pending_embedding {
        if pending > 0 {
            lines.push(String::new());
            lines.push(format!(
                "💡  {pending} doc(s) need embedding — run `onebrain qmd embed`"
            ));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_report() -> QmdStatusReport {
        QmdStatusReport {
            collection: Some("ob-1".to_string()),
            qmd_available: true,
            index: QmdStatus {
                total_files: Some(600),
                embedded_vectors: Some(7203),
                pending_embedding: Some(29),
                index_size: Some("45.7 MB".to_string()),
                last_updated: Some("1d ago".to_string()),
            },
        }
    }

    #[test]
    fn text_lists_collection_and_counts() {
        let s = render_text(&full_report());
        assert!(s.contains("🔍  Qmd index"), "{s}");
        assert!(s.contains("    Collection    ob-1"), "{s}");
        assert!(s.contains("600 indexed · 7203 embedded · 29 pending"));
        assert!(s.contains("45.7 MB"));
        assert!(s.contains("need embedding"));
        assert!(!s.trim_start().starts_with('{'), "text must not be JSON");
    }

    #[test]
    fn text_flags_unavailable_qmd() {
        let report = QmdStatusReport {
            collection: Some("ob-1".to_string()),
            qmd_available: false,
            index: QmdStatus::default(),
        };
        let s = render_text(&report);
        assert!(s.contains("not installed or not responding"));
        assert!(!s.contains("indexed ·"), "no counts when qmd unavailable");
    }

    #[test]
    fn text_flags_missing_collection() {
        let report = QmdStatusReport {
            collection: None,
            qmd_available: true,
            index: QmdStatus::default(),
        };
        let s = render_text(&report);
        assert!(s.contains("not set"));
    }

    #[test]
    fn no_embed_hint_when_zero_pending() {
        let mut report = full_report();
        report.index.pending_embedding = Some(0);
        let s = render_text(&report);
        assert!(!s.contains("need embedding"));
    }

    #[test]
    fn json_mode_emits_structured_report() {
        let line = format_output(&full_report(), &OutputMode::Json { pretty: false });
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["collection"], "ob-1");
        assert_eq!(v["qmd_available"], true);
        assert_eq!(v["total_files"], 600);
        assert_eq!(v["pending_embedding"], 29);
    }

    #[test]
    fn yaml_mode_emits_yaml_not_json() {
        let line = format_output(&full_report(), &OutputMode::Yaml);
        let v: serde_yaml::Value = serde_yaml::from_str(&line).unwrap();
        assert_eq!(v["total_files"].as_u64(), Some(600));
        assert!(!line.trim_start().starts_with('{'), "expected YAML");
    }

    #[test]
    fn from_query_none_yields_unavailable_empty_index() {
        let r = QmdStatusReport::from_query(Some("ob-1".into()), None);
        assert!(!r.qmd_available);
        assert_eq!(r.index, QmdStatus::default());
    }

    #[test]
    fn from_query_some_marks_available_and_keeps_index() {
        let r = QmdStatusReport::from_query(
            None,
            Some(QmdStatus {
                total_files: Some(5),
                ..Default::default()
            }),
        );
        assert!(r.qmd_available);
        assert_eq!(r.index.total_files, Some(5));
    }

    #[test]
    fn json_unavailable_qmd_emits_null_index_fields() {
        // Machine-consumer contract: qmd absent ⇒ qmd_available:false with the
        // flattened index fields present-but-null (not missing keys).
        let report = QmdStatusReport::from_query(Some("ob-1".into()), None);
        let line = format_output(&report, &OutputMode::Json { pretty: false });
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["qmd_available"], false);
        assert_eq!(v["collection"], "ob-1");
        assert!(v["total_files"].is_null());
        assert!(v["pending_embedding"].is_null());
    }
}
