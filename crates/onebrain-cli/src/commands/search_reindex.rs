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
    collection_cache_dir, collection_for, index_size_bytes, model_not_chosen, open_engine,
    reindex_progress_path,
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
    /// Total on-disk size in bytes of the index (the `tantivy/` and `vectors/`
    /// dirs plus `engine.redb`) AFTER this run. `None` if the size couldn't be
    /// measured.
    #[serde(skip_serializing_if = "Option::is_none")]
    index_size_bytes: Option<u64>,
    /// Change in index size (bytes) vs. before this run: positive = grew,
    /// negative = shrank. `None` if either measurement was unavailable.
    #[serde(skip_serializing_if = "Option::is_none")]
    index_size_delta_bytes: Option<i64>,
}

impl ReindexData {
    fn from_stats(s: ReindexStats, before: Option<u64>, after: Option<u64>) -> Self {
        let delta = match (before, after) {
            (Some(b), Some(a)) => Some(a as i64 - b as i64),
            _ => None,
        };
        Self {
            added: s.added,
            updated: s.updated,
            removed: s.removed,
            unchanged: s.unchanged,
            failed: s.failed,
            index_size_bytes: after,
            index_size_delta_bytes: delta,
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

    // --force: wipe the index files (NOT the downloaded models) before
    // opening, so everything re-chunks + re-embeds from scratch. Needed
    // when stored vectors go stale in ways a hash diff can't see (e.g.
    // embedding-prefix changes).
    if args.force {
        anyhow::ensure!(
            args.paths.is_empty(),
            "--force rebuilds the whole index; it can't be combined with specific paths"
        );
        wipe_index_files(vault_flag.clone())?;
    }

    let (mut engine, resolved) = open_engine(vault_flag)?;
    let vault_info = crate::vault_ctx::info_from(&resolved);

    // Measure the index size before the run so we can report the delta after.
    // Best-effort: a collection-resolution hiccup degrades to "no delta"
    // rather than failing the reindex.
    let cache_dir = collection_for(&resolved)
        .ok()
        .map(|c| collection_cache_dir(&c));
    let size_before = cache_dir.as_deref().and_then(index_size_bytes);

    // Live progress goes to STDERR (never stdout — stdout carries only the
    // envelope). Rendered only when stderr is a real TTY AND we're in text mode
    // (not `--json` / `--yaml`, which must stay silent except the final
    // envelope). Piped / agent / hook runs get a couple of plain milestone
    // lines instead of a cursor-controlled bar.
    let mut reporter = ProgressReporter::new(mode, model_load_notice_for(&resolved));

    // Live progress marker: lets a `search status` in ANOTHER process see
    // this run's (done, total) while it works. Removed on drop (any exit).
    let live = LiveProgressFile::new(&resolved)?;
    let mut on_progress = |p: ReindexProgress| {
        match &p {
            ReindexProgress::Walked { total } => live.record(0, *total),
            ReindexProgress::Indexing { done, total, .. } => live.record(*done, *total),
            ReindexProgress::LoadingModel => {}
        }
        reporter.handle(p);
    };

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
    drop(live); // remove the marker before printing the final summary

    // Re-measure after indexing so the summary can show the resulting size and
    // its delta vs. before the run.
    let size_after = cache_dir.as_deref().and_then(index_size_bytes);
    let data = ReindexData::from_stats(stats, size_before, size_after);

    let envelope = Envelope::ok("search.reindex", Some(vault_info), data);
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

/// RAII live-progress marker (see `search_common::reindex_progress_path`):
/// `record` rewrites the tiny JSON atomically; dropping removes the file so
/// a finished (or failed) reindex never leaves a "running" marker behind.
struct LiveProgressFile {
    path: PathBuf,
}

impl LiveProgressFile {
    fn new(resolved: &onebrain_core::ResolvedVault) -> Result<Self> {
        let collection = collection_for(resolved)?;
        Ok(Self {
            path: reindex_progress_path(&collection_cache_dir(&collection)),
        })
    }

    fn record(&self, done: usize, total: usize) {
        let _ = onebrain_fs::atomic_write_text(
            &self.path,
            &format!("{{\"done\":{done},\"total\":{total}}}"),
        );
    }
}

impl Drop for LiveProgressFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Delete the collection's index files — `tantivy/`, `vectors/`, and
/// `engine.redb` — while keeping the downloaded `models--*` dirs. The next
/// `Engine::open` recreates everything empty.
fn wipe_index_files(vault_flag: Option<PathBuf>) -> Result<()> {
    let resolved = crate::vault_ctx::require(vault_flag)?;
    let collection = collection_for(&resolved)?;
    let cache_dir = collection_cache_dir(&collection);
    for sub in ["tantivy", "vectors"] {
        match std::fs::remove_dir_all(cache_dir.join(sub)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    match std::fs::remove_file(cache_dir.join("engine.redb")) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
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
        println!("✅  Using {chosen} — indexing now…");
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
    Bar {
        pb: indicatif::ProgressBar,
        load_notice: String,
    },
    PlainLines {
        last_pct_bucket: Option<usize>,
        load_notice: String,
    },
    Silent,
}

impl ProgressReporter {
    fn new(mode: &OutputMode, load_notice: String) -> Self {
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
                    "📇  Indexing  {pos}/{len}  ({percent}%)  {wide_msg}",
                )
                .expect("static template is valid")
                .progress_chars("=>-"),
            );
            Self::Bar { pb, load_notice }
        } else {
            Self::PlainLines {
                last_pct_bucket: None,
                load_notice,
            }
        }
    }

    fn handle(&mut self, p: ReindexProgress) {
        match p {
            // The walk finished: the total is known before any (slow) model
            // load / embed starts, so the bar reads 0/N instead of 0/0.
            ReindexProgress::Walked { total } => match self {
                Self::Bar { pb, .. } => {
                    pb.set_length(total as u64);
                    pb.set_position(0);
                }
                Self::PlainLines { .. } => {
                    eprintln!("📇  Indexing {total} doc(s)…");
                }
                Self::Silent => {}
            },
            ReindexProgress::LoadingModel => match self {
                Self::Bar { pb, load_notice } => {
                    // fastembed prints its own download bar; suspend ours so the
                    // notice + that bar aren't clobbered by our redraw.
                    pb.suspend(|| {
                        eprintln!("{load_notice}");
                    });
                }
                Self::PlainLines { load_notice, .. } => {
                    eprintln!("{load_notice}");
                }
                Self::Silent => {}
            },
            ReindexProgress::Indexing {
                done,
                total,
                doc_path,
            } => match self {
                Self::Bar { pb, .. } => {
                    if pb.length() != Some(total as u64) {
                        pb.set_length(total as u64);
                    }
                    pb.set_position(done as u64);
                    pb.set_message(doc_path);
                }
                Self::PlainLines {
                    last_pct_bucket, ..
                } => {
                    // Throttle to ~every 10% so a piped log stays readable.
                    // `checked_div` guards total == 0 (an empty vault emits no
                    // Indexing events anyway, so this branch is really total>0).
                    if let Some(bucket) = (done * 10).checked_div(total) {
                        if *last_pct_bucket != Some(bucket) {
                            *last_pct_bucket = Some(bucket);
                            let pct = (done * 100) / total;
                            eprintln!("📇  Indexing {done}/{total} ({pct}%)");
                        }
                    }
                }
                Self::Silent => {}
            },
        }
    }

    fn finish(&mut self) {
        if let Self::Bar { pb, .. } = self {
            // Clear the bar so the final ✅  summary prints on a clean line.
            pb.finish_and_clear();
        }
    }
}

/// What to announce when the engine signals `LoadingModel`: honest about
/// whether this is a real first-run download or just loading an
/// already-downloaded model into memory (both stall the bar; only one
/// costs bandwidth).
pub(crate) fn model_load_notice(model: &str, downloaded: bool, approx_size: &str) -> String {
    if downloaded {
        format!("🧠  Loading {model} model…")
    } else {
        format!("⏬  Downloading {model} model ({approx_size}, first run)…")
    }
}

/// Best-effort wiring for [`model_load_notice`]: reads the active model from
/// vault config and checks its on-disk download status under the collection
/// cache dir. Any lookup failure degrades to the generic "loading" wording
/// (never blocks the reindex over a status probe).
fn model_load_notice_for(resolved: &onebrain_core::ResolvedVault) -> String {
    use onebrain_search::embed::{model_download_status, model_registry};

    let model = onebrain_core::load_vault_config(&resolved.root)
        .map(|c| c.search.embed_model)
        .unwrap_or_default();
    let info = model_registry().iter().find(|m| m.name == model);
    match info {
        Some(i) => {
            let downloaded = collection_for(resolved)
                .map(|c| model_download_status(i, &collection_cache_dir(&c)).downloaded)
                .unwrap_or(false);
            model_load_notice(&model, downloaded, i.approx_size)
        }
        None => format!("🧠  Loading {model} model…"),
    }
}

/// Human-readable byte size (`471 MB`, `1.2 GB`, `840 KB`, `12 B`). Matches
/// `search_status::format_size`.
fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// The `📊  index <size> (<delta>)` suffix appended to the summary line when the
/// index size is known. A zero delta renders without the parenthetical; a
/// negative delta uses a minus sign (`−0.8 MB`). Returns `None` when the size
/// couldn't be measured (so the summary just omits it).
fn index_size_suffix(size: Option<u64>, delta: Option<i64>) -> Option<String> {
    let size = size?;
    let paren = match delta {
        Some(0) | None => String::new(),
        Some(d) if d > 0 => format!(" (+{})", format_size(d as u64)),
        Some(d) => format!(" (−{})", format_size(d.unsigned_abs())),
    };
    Some(format!("📊  Index {}{paren}", format_size(size)))
}

fn render_text(env: &Envelope<ReindexData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    // Only surface non-zero categories so the line stays readable; keep a
    // fixed order. `failed` gets a ⚠️  so it stands out from the happy path.
    let mut parts: Vec<String> = Vec::new();
    if d.added > 0 {
        parts.push(format!("📄  {} added", d.added));
    }
    if d.updated > 0 {
        parts.push(format!("♻️  {} updated", d.updated));
    }
    if d.removed > 0 {
        parts.push(format!("🗑️  {} removed", d.removed));
    }
    if d.unchanged > 0 {
        parts.push(format!("⏭️  {} unchanged", d.unchanged));
    }
    if d.failed > 0 {
        parts.push(format!("⚠️  {} failed", d.failed));
    }
    let index_line = index_size_suffix(d.index_size_bytes, d.index_size_delta_bytes);

    if parts.is_empty() {
        // Nothing to do — an already-current index reindexed to a no-op.
        let mut out = "✅  Reindexed — nothing to update".to_string();
        if let Some(idx) = index_line {
            out.push_str(&format!("\n    {idx}"));
        }
        return out;
    }
    // Headline, then counts and index size each on their own indented line.
    let mut out = "✅  Reindexed".to_string();
    out.push_str(&format!("\n    {}", parts.join(" · ")));
    if let Some(idx) = index_line {
        out.push_str(&format!("\n    {idx}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_progress_file_records_json_and_removes_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reindex-progress.json");
        {
            let live = LiveProgressFile { path: path.clone() };
            live.record(457, 761);
            let body = std::fs::read_to_string(&path).unwrap();
            assert!(body.contains("\"done\":457"), "{body}");
            assert!(body.contains("\"total\":761"), "{body}");
        }
        assert!(!path.exists(), "marker removed on drop");
    }

    /// Build `ReindexData` from counts only (no index-size fields) — the size
    /// suffix is covered by its own dedicated tests.
    fn data(
        added: usize,
        updated: usize,
        removed: usize,
        unchanged: usize,
        failed: usize,
    ) -> ReindexData {
        ReindexData {
            added,
            updated,
            removed,
            unchanged,
            failed,
            index_size_bytes: None,
            index_size_delta_bytes: None,
        }
    }

    #[test]
    fn load_notice_distinguishes_download_from_load() {
        let dl = model_load_notice("bge-m3", false, "~2.2 GB");
        assert!(dl.contains("Downloading bge-m3"), "{dl}");
        assert!(dl.contains("~2.2 GB"), "{dl}");
        let ld = model_load_notice("bge-m3", true, "~2.2 GB");
        assert!(ld.contains("Loading bge-m3"), "{ld}");
        assert!(
            !ld.contains("Downloading"),
            "already-downloaded must not claim a download: {ld}"
        );
    }

    #[test]
    fn text_summarizes_all_four_counts() {
        let e = Envelope::ok("search.reindex", None, data(1, 2, 3, 4, 0));
        let s = render_text(&e);
        assert!(s.contains("✅  Reindexed"), "{s}");
        assert!(s.contains("1 added"));
        assert!(s.contains("2 updated"));
        assert!(s.contains("3 removed"));
        assert!(s.contains("4 unchanged"));
        // failed == 0 is omitted from the summary line.
        assert!(!s.contains("failed"));
    }

    #[test]
    fn text_omits_zero_categories() {
        let e = Envelope::ok("search.reindex", None, data(5, 0, 0, 700, 0));
        let s = render_text(&e);
        assert!(s.contains("5 added"));
        assert!(s.contains("700 unchanged"));
        // Zero categories are not shown.
        assert!(!s.contains("updated"));
        assert!(!s.contains("removed"));
    }

    #[test]
    fn text_reports_noop_when_all_zero() {
        let e = Envelope::ok("search.reindex", None, data(0, 0, 0, 0, 0));
        assert_eq!(render_text(&e), "✅  Reindexed — nothing to update");
    }

    #[test]
    fn text_appends_failed_count_when_nonzero() {
        let e = Envelope::ok("search.reindex", None, data(0, 0, 0, 0, 2));
        assert!(render_text(&e).contains("2 failed"));
    }

    #[test]
    fn text_appends_index_size_and_positive_delta() {
        let mut d = data(23, 3, 19, 744, 0);
        d.index_size_bytes = Some(16 * 1024 * 1024 + 200 * 1024); // ~16.2 MB
        d.index_size_delta_bytes = Some(1_300_000); // +~1.2 MB
        let e = Envelope::ok("search.reindex", None, d);
        let s = render_text(&e);
        assert!(s.contains("📊  Index 16.2 MB"), "{s}");
        assert!(s.contains("(+1.2 MB)"), "{s}");
    }

    #[test]
    fn text_renders_negative_delta_with_minus_sign() {
        let s = index_size_suffix(Some(10 * 1024 * 1024), Some(-800 * 1024)).unwrap();
        assert!(s.contains("📊  Index 10.0 MB"), "{s}");
        assert!(s.contains("(−800 KB)"), "{s}");
    }

    #[test]
    fn text_omits_parenthetical_on_zero_delta() {
        let s = index_size_suffix(Some(5 * 1024 * 1024), Some(0)).unwrap();
        assert!(s.contains("📊  Index 5.0 MB"), "{s}");
        assert!(
            !s.contains('('),
            "zero delta must render no parenthetical: {s}"
        );
    }

    #[test]
    fn text_omits_index_suffix_when_size_unknown() {
        assert!(index_size_suffix(None, Some(100)).is_none());
    }

    #[test]
    fn from_stats_computes_delta_only_when_both_known() {
        let stats = || ReindexStats {
            added: 1,
            ..Default::default()
        };
        let d = ReindexData::from_stats(stats(), Some(100), Some(180));
        assert_eq!(d.index_size_bytes, Some(180));
        assert_eq!(d.index_size_delta_bytes, Some(80));

        let d2 = ReindexData::from_stats(stats(), None, Some(180));
        assert_eq!(d2.index_size_bytes, Some(180));
        assert_eq!(d2.index_size_delta_bytes, None);

        let d3 = ReindexData::from_stats(stats(), Some(200), Some(120));
        assert_eq!(d3.index_size_delta_bytes, Some(-80));
    }
}
