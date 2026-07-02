//! `onebrain search status` — report native-search index status.
//!
//! Vault-required (exit 64 outside a vault). Reports the configured (or
//! auto-generated) collection, embed model, on-disk cache dir, the downloaded
//! model's size + download time, the last-indexed timestamp, the indexed doc
//! count, and the pending drift between the vault and the index.
//!
//! Opening the engine here is read-only: `Engine::open` uses the lazy
//! embedder (engine.rs), so it never downloads a model. `Engine::status`
//! only reads stored hashes and re-hashes the vault's `*.md` files — no
//! embed, no download — so `status` stays cheap even on a huge vault.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use crate::commands::search_common::{
    collection_cache_dir, is_indexed, open_engine, read_reindex_progress, resolve_collection,
    ReindexLiveProgress,
};
use crate::output::{emit, Envelope, OutputMode};
use onebrain_core::load_vault_config;

#[derive(Debug, Serialize)]
struct SearchStatusData {
    collection: Option<String>,
    embed_model: String,
    cache_dir: Option<PathBuf>,
    indexed: bool,
    /// Total size in bytes of the downloaded model dir(s) (`models--*`) under
    /// the collection cache dir. `None` if the model isn't downloaded yet.
    model_size_bytes: Option<u64>,
    /// Epoch seconds of the model dir's mtime (when it was downloaded).
    /// `None` if not downloaded.
    model_downloaded_at: Option<u64>,
    /// Epoch seconds of the last `reindex` run. `None` if never indexed.
    last_indexed_at: Option<u64>,
    /// Number of docs currently indexed.
    doc_count: usize,
    /// Pending drift: docs on disk with no stored hash.
    pending_new: usize,
    /// Pending drift: docs whose content changed since last index.
    pending_changed: usize,
    /// Pending drift: indexed docs whose file is gone.
    pending_removed: usize,
    /// Present when a `search reindex` is running RIGHT NOW in another
    /// process (live marker in the cache dir): its (done, total) counts.
    /// While set, `doc_count`/pending below may lag the in-flight run.
    #[serde(skip_serializing_if = "Option::is_none")]
    reindexing: Option<ReindexLiveProgress>,
}

pub fn run(vault_flag: Option<PathBuf>, mode: &OutputMode) -> Result<()> {
    let (resolved, collection) = resolve_collection(vault_flag)?;
    let vault_info = crate::vault_ctx::info_from(&resolved);
    let config = load_vault_config(&resolved.root)?;

    // Cache dir + on-disk model stats (pure fs reads — never a download).
    let (cache_dir, indexed, model_size_bytes, model_downloaded_at) = match &collection {
        Some(c) => {
            let dir = collection_cache_dir(c);
            let indexed = is_indexed(&dir);
            let (size, downloaded) = match model_dir_stats(&dir) {
                Some((size, mtime)) => (Some(size), mtime),
                None => (None, None),
            };
            (Some(dir), indexed, size, downloaded)
        }
        None => (None, false, None, None),
    };
    let reindexing = cache_dir.as_deref().and_then(read_reindex_progress);

    // Index status (doc count, last_indexed, drift) — opens the engine
    // read-only (lazy embedder → no model download) and re-hashes the vault.
    // Best-effort: if the engine can't open (shouldn't happen once a
    // collection is resolved), fall back to zeros rather than failing status.
    let (last_indexed_at, doc_count, pending_new, pending_changed, pending_removed) =
        match open_engine(Some(resolved.root.as_path().to_path_buf())) {
            Ok((engine, r)) => match engine.status(r.root.as_path()) {
                Ok(s) => (
                    s.last_indexed_at,
                    s.doc_count,
                    s.pending_new,
                    s.pending_changed,
                    s.pending_removed,
                ),
                Err(_) => (None, 0, 0, 0, 0),
            },
            Err(_) => (None, 0, 0, 0, 0),
        };

    let data = SearchStatusData {
        collection,
        embed_model: config.search.embed_model,
        cache_dir,
        indexed,
        model_size_bytes,
        model_downloaded_at,
        last_indexed_at,
        doc_count,
        pending_new,
        pending_changed,
        pending_removed,
        reindexing,
    };

    let envelope = Envelope::ok("search.status", Some(vault_info), data);
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

/// Total size in bytes + newest mtime (epoch secs) of the downloaded model
/// dir(s) (`models--*`) directly under `cache_dir`. Returns `None` when no
/// such dir exists (model not downloaded). Pure fs: no subprocess, no
/// download.
fn model_dir_stats(cache_dir: &Path) -> Option<(u64, Option<u64>)> {
    let dirs = model_dirs(cache_dir);
    if dirs.is_empty() {
        return None;
    }
    let mut total = 0u64;
    let mut newest: Option<u64> = None;
    for dir in &dirs {
        total += dir_size_bytes(dir);
        if let Some(mtime) = dir_mtime_secs(dir) {
            newest = Some(newest.map_or(mtime, |n| n.max(mtime)));
        }
    }
    Some((total, newest))
}

/// The `models--*` subdirectories directly under `cache_dir` (fastembed's
/// per-model download layout, e.g. `models--intfloat--multilingual-e5-small`).
fn model_dirs(cache_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(cache_dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("models--"))
        })
        .collect()
}

/// Recursively sum the byte sizes of every regular file under `root`
/// (hand-rolled stack walk — no new crate dep; mirrors engine.rs's
/// `walk_markdown_files`). Unreadable dirs/files are skipped.
fn dir_size_bytes(root: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(path),
                Ok(ft) if ft.is_file() => {
                    if let Ok(meta) = entry.metadata() {
                        total += meta.len();
                    }
                }
                _ => {}
            }
        }
    }
    total
}

/// `root`'s own mtime as epoch seconds, or `None` if unreadable.
fn dir_mtime_secs(root: &Path) -> Option<u64> {
    let meta = std::fs::metadata(root).ok()?;
    let modified = meta.modified().ok()?;
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Human-readable byte size (`471 MB`, `1.2 GB`, `840 KB`, `12 B`).
fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.0} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Format epoch seconds as local `YYYY-MM-DD HH:MM`, or `None` on an
/// out-of-range timestamp.
fn format_local(secs: u64) -> Option<String> {
    use chrono::TimeZone;
    match chrono::Local.timestamp_opt(secs as i64, 0) {
        chrono::LocalResult::Single(dt) => Some(dt.format("%Y-%m-%d %H:%M").to_string()),
        _ => None,
    }
}

fn render_text(env: &Envelope<SearchStatusData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    let mut lines = Vec::new();
    match &d.collection {
        Some(c) => lines.push(format!("🔍  collection    {c}")),
        None => lines.push(
            "🔍  collection    not set\n💡  set `search.collection` in onebrain.yml".to_string(),
        ),
    }
    lines.push(format!("🧠  model         {}", d.embed_model));

    match d.model_size_bytes {
        Some(size) => {
            lines.push(format!("📦 model size    {}", format_size(size)));
            if let Some(downloaded) = d.model_downloaded_at.and_then(format_local) {
                lines.push(format!("⏬  downloaded    {downloaded}"));
            }
        }
        None => lines.push("📦 model size    not downloaded".to_string()),
    }

    if let Some(dir) = &d.cache_dir {
        lines.push(format!("📁  cache         {}", dir.display()));
    }

    match d.last_indexed_at.and_then(format_local) {
        Some(when) => lines.push(format!("🕐 last indexed  {when}")),
        None => lines.push("🕐 last indexed  never".to_string()),
    }

    if let Some(r) = &d.reindexing {
        let pct = (r.done * 100).checked_div(r.total).unwrap_or(0);
        lines.push(format!(
            "🔄  reindexing    {}/{} ({pct}%) — running now; counts below may lag",
            r.done, r.total
        ));
    }

    lines.push(format!("✅  indexed  {} docs", d.doc_count));

    let pending = d.pending_new + d.pending_changed + d.pending_removed;
    if pending > 0 {
        lines.push(format!(
            "⚠️  {pending} pending ({} new · {} changed · {} removed) → run `onebrain search reindex`",
            d.pending_new, d.pending_changed, d.pending_removed
        ));
    } else {
        lines.push("✅  up to date".to_string());
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn env(collection: Option<&str>, indexed: bool) -> Envelope<SearchStatusData> {
        Envelope::ok(
            "search.status",
            None,
            SearchStatusData {
                collection: collection.map(str::to_string),
                embed_model: "multilingual-e5-small".to_string(),
                cache_dir: collection.map(|c| PathBuf::from(format!("/cache/{c}"))),
                indexed,
                model_size_bytes: None,
                model_downloaded_at: None,
                last_indexed_at: None,
                doc_count: 0,
                pending_new: 0,
                pending_changed: 0,
                pending_removed: 0,
                reindexing: None,
            },
        )
    }

    #[test]
    fn text_shows_collection_and_model() {
        let s = render_text(&env(Some("ob-1"), true));
        assert!(s.contains("🔍  collection    ob-1"));
        assert!(s.contains("🧠  model         multilingual-e5-small"));
        assert!(s.contains("✅  indexed  0 docs"));
    }

    #[test]
    fn text_flags_missing_collection() {
        let s = render_text(&env(None, false));
        assert!(s.contains("not set"));
        assert!(s.contains("💡  set `search.collection`"));
    }

    #[test]
    fn text_shows_not_downloaded_and_never_indexed_when_absent() {
        let s = render_text(&env(Some("ob-1"), false));
        assert!(s.contains("📦 model size    not downloaded"));
        assert!(s.contains("🕐 last indexed  never"));
        // No `⏬  downloaded` line when the model isn't present.
        assert!(!s.contains("⏬  downloaded"));
    }

    #[test]
    fn text_shows_up_to_date_when_no_drift() {
        let s = render_text(&env(Some("ob-1"), true));
        assert!(s.contains("✅  up to date"));
        assert!(!s.contains("pending"));
    }

    #[test]
    fn text_shows_pending_drift_when_present() {
        let mut e = env(Some("ob-1"), true);
        {
            let d = e.data.as_mut().unwrap();
            d.doc_count = 5;
            d.pending_new = 2;
            d.pending_changed = 1;
            d.pending_removed = 3;
        }
        let s = render_text(&e);
        assert!(s.contains("✅  indexed  5 docs"));
        assert!(s.contains("⚠️"));
        assert!(s.contains("6 pending (2 new · 1 changed · 3 removed)"));
        assert!(s.contains("onebrain search reindex"));
        assert!(!s.contains("up to date"));
    }

    #[test]
    fn text_shows_model_size_and_download_time_when_present() {
        let mut e = env(Some("ob-1"), true);
        {
            let d = e.data.as_mut().unwrap();
            d.model_size_bytes = Some(471 * 1024 * 1024);
            d.model_downloaded_at = Some(1_700_000_000);
        }
        let s = render_text(&e);
        assert!(s.contains("📦 model size    471 MB"));
        assert!(s.contains("⏬  downloaded    "));
    }

    #[test]
    fn json_shape_has_new_and_existing_fields() {
        let mut e = env(Some("ob-1"), true);
        {
            let d = e.data.as_mut().unwrap();
            d.doc_count = 7;
            d.pending_new = 1;
            d.last_indexed_at = Some(1_700_000_000);
            d.model_size_bytes = Some(1234);
        }
        let v = serde_json::to_value(e.data.as_ref().unwrap()).unwrap();
        // Existing fields preserved.
        assert_eq!(v["collection"], "ob-1");
        assert!(v["embed_model"].is_string());
        assert_eq!(v["indexed"], true);
        // New fields added.
        assert_eq!(v["doc_count"], 7);
        assert_eq!(v["pending_new"], 1);
        assert_eq!(v["pending_changed"], 0);
        assert_eq!(v["pending_removed"], 0);
        assert_eq!(v["last_indexed_at"], 1_700_000_000u64);
        assert_eq!(v["model_size_bytes"], 1234);
    }

    #[test]
    fn text_shows_live_reindex_line_when_marker_present() {
        let mut e = env(Some("ob-1"), true);
        e.data.as_mut().unwrap().reindexing = Some(ReindexLiveProgress {
            done: 457,
            total: 761,
        });
        let s = render_text(&e);
        assert!(s.contains("🔄  reindexing    457/761 (60%)"), "{s}");
        assert!(s.contains("counts below may lag"), "{s}");
    }

    #[test]
    fn json_omits_reindexing_when_absent() {
        let v = serde_json::to_value(env(Some("ob-1"), true).data.as_ref().unwrap()).unwrap();
        assert!(v.get("reindexing").is_none());
    }

    #[test]
    fn format_size_renders_units() {
        assert_eq!(format_size(12), "12 B");
        assert_eq!(format_size(2048), "2 KB");
        assert_eq!(format_size(471 * 1024 * 1024), "471 MB");
        assert_eq!(
            format_size(2 * 1024 * 1024 * 1024 + 200 * 1024 * 1024),
            "2.2 GB"
        );
    }

    #[test]
    fn model_dir_stats_none_when_no_model_dir() {
        let dir = tempdir().unwrap();
        // A cache dir with unrelated subdirs but no `models--*`.
        std::fs::create_dir(dir.path().join("tantivy")).unwrap();
        std::fs::create_dir(dir.path().join("vectors")).unwrap();
        assert!(model_dir_stats(dir.path()).is_none());
    }

    #[test]
    fn model_dir_stats_sums_sizes_across_model_dir() {
        let dir = tempdir().unwrap();
        let model = dir.path().join("models--intfloat--multilingual-e5-small");
        std::fs::create_dir_all(model.join("snapshots/abc")).unwrap();
        std::fs::write(model.join("snapshots/abc/model.onnx"), vec![0u8; 1000]).unwrap();
        std::fs::write(model.join("config.json"), vec![0u8; 24]).unwrap();
        // A non-model sibling dir must NOT be counted.
        std::fs::create_dir(dir.path().join("tantivy")).unwrap();
        std::fs::write(dir.path().join("tantivy/meta.json"), vec![0u8; 5000]).unwrap();

        let (size, mtime) = model_dir_stats(dir.path()).unwrap();
        assert_eq!(size, 1024, "should sum only the model dir's files");
        assert!(mtime.is_some());
    }

    #[test]
    fn format_local_formats_or_none() {
        // A normal timestamp renders `YYYY-MM-DD HH:MM`.
        let s = format_local(1_700_000_000).unwrap();
        assert_eq!(s.len(), "2023-11-14 22:13".len());
        assert!(s.contains('-') && s.contains(':'));
    }
}
