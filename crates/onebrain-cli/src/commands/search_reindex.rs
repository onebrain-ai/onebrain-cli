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
use crate::commands::search_common::{
    collection_cache_dir, collection_for, model_not_chosen, open_engine,
};
use crate::output::{emit, Envelope, OutputMode};
use onebrain_search::engine::{ReindexProgress, ReindexStats};

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
    // First-run model choice: if no model has been chosen yet (no persisted
    // `search.embed_model` key AND nothing downloaded), prompt the user to pick
    // one BEFORE `open_engine` triggers a download — but only on a real TTY in
    // text mode. Non-TTY / structured runs (pipes, agents, hooks, scheduled
    // reindex) silently keep the `multilingual-e5-small` default so a headless
    // reindex never blocks on a prompt. If a model is already chosen/downloaded,
    // no prompt fires.
    maybe_prompt_first_model(vault_flag.clone(), mode)?;

    let (mut engine, resolved) = open_engine(vault_flag)?;
    let vault_info = crate::vault_ctx::info_from(&resolved);

    // Live progress goes to STDERR (never stdout — stdout carries only the
    // envelope). Rendered only when stderr is a real TTY AND we're in text mode
    // (not `--json` / `--yaml`, which must stay silent except the final
    // envelope). Piped / agent / hook runs get a couple of plain milestone
    // lines instead of a cursor-controlled bar.
    let mut reporter = ProgressReporter::new(mode);
    let mut on_progress = |p: ReindexProgress| reporter.handle(p);

    let stats = if args.paths.is_empty() {
        engine.reindex_all_with_progress(resolved.root.as_path(), &mut on_progress)?
    } else {
        engine.reindex_paths_with_progress(
            resolved.root.as_path(),
            &args.paths,
            &mut on_progress,
        )?
    };
    reporter.finish();

    let envelope = Envelope::ok("search.reindex", Some(vault_info), ReindexData::from(stats));
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

/// First-run model prompt (Feature B). On a real TTY in text mode, when no
/// model has been chosen yet, ask the user to pick one via the shared
/// `inquire::Select` picker and persist the choice to `onebrain.yml` BEFORE the
/// reindex downloads anything. Any other case (already chosen, non-TTY,
/// structured output) is a silent no-op — the caller proceeds with the
/// configured/default model.
///
/// Persists the choice directly (not via `apply_model_change`, which would open
/// the old index + rebuild — pointless on a first run with nothing indexed
/// yet): the subsequent normal reindex downloads + embeds the chosen model
/// exactly once.
fn maybe_prompt_first_model(vault_flag: Option<PathBuf>, mode: &OutputMode) -> Result<()> {
    use std::io::IsTerminal;

    // Structured output or non-TTY → never prompt (headless-safe default).
    if mode.is_structured() || !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Ok(());
    }

    let resolved = crate::vault_ctx::require(vault_flag)?;
    let collection = collection_for(&resolved)?;
    let cache_dir = collection_cache_dir(&collection);

    if !model_not_chosen(resolved.root.as_path(), &cache_dir) {
        return Ok(()); // A model is already chosen or downloaded.
    }

    // Prompt starting on the default model; Esc/cancel keeps the default.
    let config = onebrain_core::load_vault_config(&resolved.root)?;
    let current = config.search.embed_model.clone();
    if let Some(chosen) = crate::commands::search_model::prompt_pick_model(&current) {
        onebrain_fs::persist_search_key(resolved.root.as_path(), "embed_model", chosen)?;
        println!("✅ using {chosen} — indexing now…");
    }
    Ok(())
}

/// How live reindex progress is surfaced, chosen once from the output mode +
/// stderr TTY state:
/// - `Bar` — an in-place `indicatif` progress bar on a real TTY in text mode.
/// - `PlainLines` — a few plain milestone lines to stderr on a non-TTY text run
///   (piped / agent / hook): the model-load notice plus a ~10%-throttled counter
///   and a final "indexed N" line, so a log never gets thousands of lines.
/// - `Silent` — nothing (structured `--json` / `--yaml`: only the final
///   envelope is emitted).
enum ProgressReporter {
    Bar(indicatif::ProgressBar),
    PlainLines { last_pct_bucket: Option<usize> },
    Silent,
}

impl ProgressReporter {
    fn new(mode: &OutputMode) -> Self {
        use is_terminal::IsTerminal;
        // Structured modes stay silent regardless of TTY.
        if mode.is_structured() {
            return Self::Silent;
        }
        if std::io::stderr().is_terminal() {
            let pb = indicatif::ProgressBar::new(0);
            // Bar while total is known; the template degrades gracefully before
            // the first `set_length` (spinner-ish) too.
            pb.set_style(
                indicatif::ProgressStyle::with_template(
                    "📇 indexing  {pos}/{len}  ({percent}%)  {wide_msg}",
                )
                .expect("static template is valid")
                .progress_chars("=>-"),
            );
            Self::Bar(pb)
        } else {
            Self::PlainLines {
                last_pct_bucket: None,
            }
        }
    }

    fn handle(&mut self, p: ReindexProgress) {
        match p {
            ReindexProgress::LoadingModel => match self {
                Self::Bar(pb) => {
                    // fastembed prints its own download bar; suspend ours so the
                    // notice + that bar aren't clobbered by our redraw.
                    pb.suspend(|| {
                        eprintln!("⏬ downloading embedding model (~470 MB, first run)…");
                    });
                }
                Self::PlainLines { .. } => {
                    eprintln!("⏬ downloading embedding model (~470 MB, first run)…");
                }
                Self::Silent => {}
            },
            ReindexProgress::Indexing {
                done,
                total,
                doc_path,
            } => match self {
                Self::Bar(pb) => {
                    if pb.length() != Some(total as u64) {
                        pb.set_length(total as u64);
                    }
                    pb.set_position(done as u64);
                    pb.set_message(doc_path);
                }
                Self::PlainLines { last_pct_bucket } => {
                    // Throttle to ~every 10% so a piped log stays readable.
                    // `checked_div` guards total == 0 (an empty vault emits no
                    // Indexing events anyway, so this branch is really total>0).
                    if let Some(bucket) = (done * 10).checked_div(total) {
                        if *last_pct_bucket != Some(bucket) {
                            *last_pct_bucket = Some(bucket);
                            let pct = (done * 100) / total;
                            eprintln!("📇 indexing {done}/{total} ({pct}%)");
                        }
                    }
                }
                Self::Silent => {}
            },
        }
    }

    fn finish(&mut self) {
        if let Self::Bar(pb) = self {
            // Clear the bar so the final ✅ summary prints on a clean line.
            pb.finish_and_clear();
        }
    }
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
