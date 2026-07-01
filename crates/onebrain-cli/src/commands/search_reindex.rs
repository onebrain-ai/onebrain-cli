//! `onebrain search reindex` — reindex the whole vault, or specific doc
//! paths.
//!
//! Vault-required (exit 64 outside a vault). This is the expected
//! first-model-download point: `Engine::index_doc` embeds every new/changed
//! chunk, so the lazy embedder (engine.rs) constructs here on first call.

use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;

use crate::cli::SearchReindexArgs;
use crate::commands::search_common::open_engine;
use crate::output::{emit, Envelope, OutputMode};
use onebrain_search::engine::ReindexStats;

#[derive(Debug, Serialize)]
struct ReindexData {
    added: usize,
    updated: usize,
    removed: usize,
    unchanged: usize,
    failed: usize,
}

impl From<ReindexStats> for ReindexData {
    fn from(s: ReindexStats) -> Self {
        Self {
            added: s.added,
            updated: s.updated,
            removed: s.removed,
            unchanged: s.unchanged,
            failed: s.failed,
        }
    }
}

pub fn run(vault_flag: Option<PathBuf>, mode: &OutputMode, args: &SearchReindexArgs) -> Result<()> {
    let (mut engine, resolved) = open_engine(vault_flag)?;
    let vault_info = crate::vault_ctx::info_from(&resolved);

    let stats = if args.paths.is_empty() {
        engine.reindex_all(resolved.root.as_path())?
    } else {
        engine.reindex_paths(resolved.root.as_path(), &args.paths)?
    };

    let envelope = Envelope::ok("search.reindex", Some(vault_info), ReindexData::from(stats));
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

fn render_text(env: &Envelope<ReindexData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    // Only surface non-zero categories so the line stays readable; keep a
    // fixed order. `failed` gets a ⚠️ so it stands out from the happy path.
    let mut parts: Vec<String> = Vec::new();
    if d.added > 0 {
        parts.push(format!("📄 {} added", d.added));
    }
    if d.updated > 0 {
        parts.push(format!("♻️ {} updated", d.updated));
    }
    if d.removed > 0 {
        parts.push(format!("🗑️ {} removed", d.removed));
    }
    if d.unchanged > 0 {
        parts.push(format!("⏭️ {} unchanged", d.unchanged));
    }
    if d.failed > 0 {
        parts.push(format!("⚠️ {} failed", d.failed));
    }

    if parts.is_empty() {
        // Nothing to do — an already-current index reindexed to a no-op.
        return "✅ reindexed — nothing to update".to_string();
    }
    format!("✅ reindexed — {}", parts.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_summarizes_all_four_counts() {
        let e = Envelope::ok(
            "search.reindex",
            None,
            ReindexData {
                added: 1,
                updated: 2,
                removed: 3,
                unchanged: 4,
                failed: 0,
            },
        );
        let s = render_text(&e);
        assert!(s.contains("✅ reindexed"));
        assert!(s.contains("1 added"));
        assert!(s.contains("2 updated"));
        assert!(s.contains("3 removed"));
        assert!(s.contains("4 unchanged"));
        // failed == 0 is omitted from the summary line.
        assert!(!s.contains("failed"));
    }

    #[test]
    fn text_omits_zero_categories() {
        let e = Envelope::ok(
            "search.reindex",
            None,
            ReindexData {
                added: 5,
                updated: 0,
                removed: 0,
                unchanged: 700,
                failed: 0,
            },
        );
        let s = render_text(&e);
        assert!(s.contains("5 added"));
        assert!(s.contains("700 unchanged"));
        // Zero categories are not shown.
        assert!(!s.contains("updated"));
        assert!(!s.contains("removed"));
    }

    #[test]
    fn text_reports_noop_when_all_zero() {
        let e = Envelope::ok(
            "search.reindex",
            None,
            ReindexData {
                added: 0,
                updated: 0,
                removed: 0,
                unchanged: 0,
                failed: 0,
            },
        );
        assert_eq!(render_text(&e), "✅ reindexed — nothing to update");
    }

    #[test]
    fn text_appends_failed_count_when_nonzero() {
        let e = Envelope::ok(
            "search.reindex",
            None,
            ReindexData {
                added: 0,
                updated: 0,
                removed: 0,
                unchanged: 0,
                failed: 2,
            },
        );
        assert!(render_text(&e).contains("2 failed"));
    }
}
