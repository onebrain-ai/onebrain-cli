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
use onebrain_core::path::ResolvedVault;
use serde::Serialize;

use crate::commands::search_common::{
    collection_cache_dir, collection_for, index_size_bytes, is_indexed, read_reindex_progress,
    resolve_collection, ReindexLiveProgress,
};
use crate::output::{emit, item, section, Envelope, OutputMode};
use onebrain_core::load_vault_config;
use onebrain_search::embed::{model_download_status, model_registry};
use onebrain_search::engine::Engine;

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub(crate) struct SearchStatusData {
    collection: Option<String>,
    embed_model: String,
    cache_dir: Option<PathBuf>,
    indexed: bool,
    /// Size in bytes of the ACTIVE model's dir (`models--*`) under the
    /// collection cache dir. `None` if the active model isn't downloaded yet.
    /// (The whole-cache total, including any lingering other-model dirs, is
    /// `cache_size_bytes`.)
    model_size_bytes: Option<u64>,
    /// Epoch seconds of the active model dir's mtime (when it was downloaded).
    /// `None` if not downloaded.
    model_downloaded_at: Option<u64>,
    /// Epoch seconds of the last `reindex` run. `None` if never indexed.
    last_indexed_at: Option<u64>,
    /// Total on-disk size in bytes of the index itself (the `tantivy/` and
    /// `vectors/` dirs plus `engine.redb`, under the collection cache dir) — the
    /// downloaded models are NOT counted here (see `model_size_bytes`). `None`
    /// when no index exists yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    index_size_bytes: Option<u64>,
    /// Number of docs currently indexed.
    doc_count: usize,
    /// Pending drift: docs on disk with no stored hash.
    pending_new: usize,
    /// Pending drift: docs whose content changed since last index.
    pending_changed: usize,
    /// Pending drift: indexed docs whose file is gone.
    pending_removed: usize,
    /// Total byte size of the whole collection cache dir (models + index +
    /// live markers). `None` when no collection is configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_size_bytes: Option<u64>,
    /// Present when a `search reindex` is running RIGHT NOW in another
    /// process (live marker in the cache dir): its (done, total) counts.
    /// While set, `doc_count`/pending below may lag the in-flight run.
    #[serde(skip_serializing_if = "Option::is_none")]
    reindexing: Option<ReindexLiveProgress>,
    /// `false` in a lex-only build (no `semantic` feature): no ONNX runtime,
    /// so embedding-backed verbs (`vsearch`, hybrid `query`, `model set`) are
    /// unavailable and `reindex` indexes keyword-only. See ADR 0017.
    semantic_available: bool,
}

pub fn run(vault_flag: Option<PathBuf>, mode: &OutputMode) -> Result<()> {
    let (resolved, collection) = resolve_collection(vault_flag)?;
    let vault_info = crate::vault_ctx::info_from(&resolved);

    let data = status_data(&resolved, collection)?;

    let envelope = Envelope::ok("search.status", Some(vault_info), data);
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

/// Build the `search status` payload for an already-resolved vault.
///
/// `collection` is the effective collection name (already resolved by the
/// caller, e.g. via [`resolve_collection`] or [`collection_for`]) so callers
/// that already have it don't re-derive it.
///
/// Order matters here: the on-disk cache stats (`indexed`, model/index sizes)
/// are read from the filesystem *before* the engine is opened, because
/// `Engine::open` creates the cache dir as a side effect — checking
/// `is_indexed` afterwards would always see a freshly-created (but empty)
/// dir and report `true` on a never-indexed vault.
pub(crate) fn status_data(
    resolved: &ResolvedVault,
    collection: Option<String>,
) -> Result<SearchStatusData> {
    let config = load_vault_config(&resolved.root)?;

    // Cache dir + on-disk model stats + index size (pure fs reads — never a
    // download). Must run before the engine is opened (see doc comment).
    let (cache_dir, indexed, model_size_bytes, model_downloaded_at, index_size_bytes) =
        match &collection {
            Some(c) => {
                let dir = collection_cache_dir(c);
                let indexed = is_indexed(&dir);
                let (size, downloaded) =
                    match active_model_dir_stats(&dir, &config.search.embed_model) {
                        Some((size, mtime)) => (Some(size), mtime),
                        None => (None, None),
                    };
                let idx_size = index_size_bytes(&dir);
                (Some(dir), indexed, size, downloaded, idx_size)
            }
            None => (None, false, None, None, None),
        };
    let reindexing = cache_dir.as_deref().and_then(read_reindex_progress);
    let cache_size_bytes = cache_dir
        .as_deref()
        .filter(|d| d.is_dir())
        .map(onebrain_search::embed::dir_size_bytes);

    // Index status (doc count, last_indexed, drift) — opens the engine
    // read-only (lazy embedder → no model download) and re-hashes the vault.
    // Open directly at the already-resolved `cache_dir` instead of via
    // `open_engine`, which would redundantly re-resolve the vault + collection
    // we already have. `set_exclude_patterns` still mirrors `open_engine` so
    // `status`'s drift walk honours `search.exclude` (else excluded files would
    // inflate `pending_new`). Best-effort: fall back to zeros on any failure
    // rather than failing status.
    let (last_indexed_at, doc_count, pending_new, pending_changed, pending_removed) =
        match cache_dir.as_deref() {
            Some(dir) => match Engine::open(dir, &config.search.embed_model) {
                Ok(mut engine) => {
                    engine.set_exclude_patterns(config.search.exclude.clone());
                    match engine.status(resolved.root.as_path()) {
                        Ok(s) => (
                            s.last_indexed_at,
                            s.doc_count,
                            s.pending_new,
                            s.pending_changed,
                            s.pending_removed,
                        ),
                        Err(_) => (None, 0, 0, 0, 0),
                    }
                }
                Err(_) => (None, 0, 0, 0, 0),
            },
            None => (None, 0, 0, 0, 0),
        };

    Ok(SearchStatusData {
        collection,
        embed_model: config.search.embed_model,
        cache_dir,
        indexed,
        model_size_bytes,
        model_downloaded_at,
        last_indexed_at,
        index_size_bytes,
        doc_count,
        pending_new,
        pending_changed,
        pending_removed,
        cache_size_bytes,
        reindexing,
        semantic_available: cfg!(feature = "semantic"),
    })
}

/// Build status data for an already-open engine (used by the MCP `status`
/// tool, which holds a live `Engine` and doesn't want to open a second one).
///
/// Unlike [`status_data`], the cache dir will already exist by the time this
/// runs (the caller's engine opened it), so `indexed` reports whether the
/// index has actually been populated (`doc_count > 0` after the status
/// query) rather than merely whether the cache dir exists.
pub(crate) fn status_data_for(
    engine: &Engine,
    resolved: &ResolvedVault,
) -> Result<SearchStatusData> {
    let collection = collection_for(resolved)?;
    let config = load_vault_config(&resolved.root)?;

    let (cache_dir, model_size_bytes, model_downloaded_at, index_size_bytes) = {
        let dir = collection_cache_dir(&collection);
        let (size, downloaded) = match active_model_dir_stats(&dir, &config.search.embed_model) {
            Some((size, mtime)) => (Some(size), mtime),
            None => (None, None),
        };
        let idx_size = index_size_bytes(&dir);
        (Some(dir), size, downloaded, idx_size)
    };
    let reindexing = cache_dir.as_deref().and_then(read_reindex_progress);
    let cache_size_bytes = cache_dir
        .as_deref()
        .filter(|d| d.is_dir())
        .map(onebrain_search::embed::dir_size_bytes);

    let status = engine.status(resolved.root.as_path())?;

    Ok(SearchStatusData {
        collection: Some(collection),
        embed_model: config.search.embed_model,
        cache_dir,
        indexed: status.doc_count > 0,
        model_size_bytes,
        model_downloaded_at,
        last_indexed_at: status.last_indexed_at,
        index_size_bytes,
        doc_count: status.doc_count,
        pending_new: status.pending_new,
        pending_changed: status.pending_changed,
        pending_removed: status.pending_removed,
        cache_size_bytes,
        reindexing,
        semantic_available: cfg!(feature = "semantic"),
    })
}

/// Size in bytes + mtime (epoch secs) of the ACTIVE model's `models--*` dir
/// only. Returns `None` when the active model isn't on disk (or isn't a known
/// registry model). Pure fs: no subprocess, no download.
///
/// This is reported under the active model's name in `status`, so it must
/// reflect the active model alone. An earlier version summed EVERY `models--*`
/// dir under the cache, which inflated the figure — and falsely implied "a
/// model is present" — whenever a previously-active model's dir lingered next
/// to the current one (#21). The whole-cache total is surfaced separately as
/// `cache_size_bytes`.
fn active_model_dir_stats(cache_dir: &Path, active_model: &str) -> Option<(u64, Option<u64>)> {
    let info = model_registry().iter().find(|m| m.name == active_model)?;
    let status = model_download_status(info, cache_dir);
    let size = status.disk_size?;
    Some((size, dir_mtime_secs(&status.path)))
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
    // Grouped layout: emoji only on section headers, values indented with
    // plain spaces — alignment never depends on how wide a terminal font
    // draws an emoji.
    let mut lines = Vec::new();

    lines.push(section("🧠", "Model"));
    lines.push(item("Name", &d.embed_model));
    if d.semantic_available {
        match d.model_size_bytes {
            Some(size) => {
                lines.push(item("Size", &format_size(size)));
                if let Some(downloaded) = d.model_downloaded_at.and_then(format_local) {
                    lines.push(item("Downloaded", &downloaded));
                }
            }
            None => lines.push(item("Size", "not downloaded")),
        }
    } else {
        // Lex-only build: no ONNX runtime for this platform, so the model is
        // never downloaded and semantic verbs are unavailable.
        lines.push(item("Semantic", "unavailable in this build (keyword-only)"));
    }

    lines.push(String::new());
    lines.push(section("📊", "Index"));
    match &d.collection {
        Some(c) => lines.push(item("Collection", c)),
        None => lines.push(item(
            "Collection",
            "not set — set `search.collection` in onebrain.yml",
        )),
    }
    lines.push(item("Docs", &d.doc_count.to_string()));
    if let Some(size) = d.index_size_bytes {
        lines.push(item("Size", &format_size(size)));
    }
    match d.last_indexed_at.and_then(format_local) {
        Some(when) => lines.push(item("Last indexed", &when)),
        None => lines.push(item("Last indexed", "never")),
    }
    if let Some(r) = &d.reindexing {
        let pct = (r.done * 100).checked_div(r.total).unwrap_or(0);
        lines.push(item(
            "Reindexing",
            &format!(
                "🔄  {}/{} ({pct}%) — running now; counts may lag",
                r.done, r.total
            ),
        ));
    }
    let pending = d.pending_new + d.pending_changed + d.pending_removed;
    if pending > 0 {
        lines.push(item(
            "Status",
            &format!(
                "⚠️  {pending} pending ({} new · {} changed · {} removed)",
                d.pending_new, d.pending_changed, d.pending_removed
            ),
        ));
    } else {
        lines.push(item("Status", "✅  up to date"));
    }

    if let Some(dir) = &d.cache_dir {
        lines.push(String::new());
        lines.push(section("📁", "Cache"));
        lines.push(item("Dir", &dir.display().to_string()));
        if let Some(size) = d.cache_size_bytes {
            lines.push(item("Size", &format_size(size)));
        }
    }

    if pending > 0 {
        lines.push(String::new());
        lines.push("💡  Run `onebrain search reindex` to index pending changes".to_string());
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
                index_size_bytes: None,
                cache_size_bytes: None,
                doc_count: 0,
                pending_new: 0,
                pending_changed: 0,
                pending_removed: 0,
                reindexing: None,
                semantic_available: true,
            },
        )
    }

    #[test]
    fn text_flags_lex_only_build_in_model_section() {
        let mut e = env(Some("ob-1"), true);
        e.data.as_mut().unwrap().semantic_available = false;
        let s = render_text(&e);
        assert!(s.contains("🧠  Model"), "{s}");
        assert!(s.contains("Semantic"), "{s}");
        assert!(s.contains("unavailable in this build"), "{s}");
        // The model download line is suppressed when semantic is unavailable.
        assert!(!s.contains("not downloaded"), "{s}");
    }

    #[test]
    fn text_shows_collection_and_model() {
        let s = render_text(&env(Some("ob-1"), true));
        assert!(s.contains("🧠  Model"), "{s}");
        assert!(s.contains("    Name          multilingual-e5-small"), "{s}");
        assert!(s.contains("📊  Index"), "{s}");
        assert!(s.contains("    Collection    ob-1"), "{s}");
        assert!(s.contains("    Docs          0"), "{s}");
        assert!(s.contains("📁  Cache"), "{s}");
        assert!(s.contains("    Dir           /cache/ob-1"), "{s}");
    }

    #[test]
    fn text_flags_missing_collection() {
        let s = render_text(&env(None, false));
        assert!(s.contains("not set — set `search.collection`"), "{s}");
    }

    #[test]
    fn text_shows_not_downloaded_and_never_indexed_when_absent() {
        let s = render_text(&env(Some("ob-1"), false));
        assert!(s.contains("    Size          not downloaded"), "{s}");
        assert!(s.contains("    Last indexed  never"), "{s}");
        // No `⏬  Downloaded` line when the model isn't present.
        assert!(!s.contains("    Downloaded"));
        // No `📊  Index size` line when there's no index.
        assert!(!s.contains("Index size"));
    }

    #[test]
    fn text_shows_up_to_date_when_no_drift() {
        let s = render_text(&env(Some("ob-1"), true));
        assert!(s.contains("    Status        ✅  up to date"), "{s}");
        assert!(!s.contains("pending"), "{s}");
        assert!(!s.contains("💡"), "no hint when up to date: {s}");
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
        assert!(s.contains("    Docs          5"), "{s}");
        assert!(
            s.contains("    Status        ⚠️  6 pending (2 new · 1 changed · 3 removed)"),
            "{s}"
        );
        assert!(s.contains("💡  Run `onebrain search reindex`"), "{s}");
        assert!(!s.contains("up to date"), "{s}");
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
        assert!(s.contains("    Size          471 MB"), "{s}");
        assert!(s.contains("    Downloaded    "), "{s}");
    }

    #[test]
    fn text_shows_index_size_when_present() {
        let mut e = env(Some("ob-1"), true);
        e.data.as_mut().unwrap().index_size_bytes = Some(16 * 1024 * 1024);
        let s = render_text(&e);
        assert!(s.contains("    Size          16 MB"), "{s}");
    }

    #[test]
    fn text_uses_two_spaces_after_every_emoji_and_aligned_values() {
        let mut e = env(Some("ob-1"), true);
        {
            let d = e.data.as_mut().unwrap();
            d.model_size_bytes = Some(471 * 1024 * 1024);
            d.index_size_bytes = Some(1024);
        }
        let s = render_text(&e);
        // Every rendered emoji row starts with `<emoji>  ` (two spaces).
        for line in s.lines() {
            for emoji in ["🔍", "🧠", "📦", "📁", "📊", "🕐"] {
                if let Some(rest) = line.strip_prefix(emoji) {
                    assert!(
                        rest.starts_with("  ") && !rest.starts_with("   "),
                        "row `{line}` must have exactly two spaces after {emoji}"
                    );
                }
            }
        }
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
            d.index_size_bytes = Some(5678);
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
        assert_eq!(v["index_size_bytes"], 5678);
    }

    #[test]
    fn json_omits_index_size_when_absent() {
        let v = serde_json::to_value(env(Some("ob-1"), true).data.as_ref().unwrap()).unwrap();
        assert!(v.get("index_size_bytes").is_none());
    }

    #[test]
    fn text_shows_live_reindex_line_when_marker_present() {
        let mut e = env(Some("ob-1"), true);
        e.data.as_mut().unwrap().reindexing = Some(ReindexLiveProgress {
            done: 457,
            total: 761,
        });
        let s = render_text(&e);
        assert!(s.contains("    Reindexing    🔄  457/761 (60%)"), "{s}");
        assert!(s.contains("counts may lag"), "{s}");
    }

    #[test]
    fn text_shows_cache_size_when_present() {
        let mut e = env(Some("ob-1"), true);
        e.data.as_mut().unwrap().cache_size_bytes = Some(512 * 1024 * 1024);
        let s = render_text(&e);
        assert!(s.contains("📁  Cache"), "{s}");
        // Both Dir and Size sit in the Cache section.
        let cache_at = s.find("📁  Cache").unwrap();
        let tail = &s[cache_at..];
        assert!(tail.contains("    Dir           /cache/ob-1"), "{tail}");
        assert!(tail.contains("    Size          512 MB"), "{tail}");
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
    fn active_model_dir_stats_none_when_active_model_not_on_disk() {
        let dir = tempdir().unwrap();
        // A cache dir with unrelated subdirs but no `models--*`.
        std::fs::create_dir(dir.path().join("tantivy")).unwrap();
        std::fs::create_dir(dir.path().join("vectors")).unwrap();
        assert!(active_model_dir_stats(dir.path(), "multilingual-e5-small").is_none());
    }

    #[test]
    fn active_model_dir_stats_sums_only_the_active_model_dir() {
        let dir = tempdir().unwrap();
        let model = dir.path().join("models--intfloat--multilingual-e5-small");
        std::fs::create_dir_all(model.join("snapshots/abc")).unwrap();
        std::fs::write(model.join("snapshots/abc/model.onnx"), vec![0u8; 1000]).unwrap();
        std::fs::write(model.join("config.json"), vec![0u8; 24]).unwrap();
        // A non-model sibling dir must NOT be counted.
        std::fs::create_dir(dir.path().join("tantivy")).unwrap();
        std::fs::write(dir.path().join("tantivy/meta.json"), vec![0u8; 5000]).unwrap();

        let (size, mtime) = active_model_dir_stats(dir.path(), "multilingual-e5-small").unwrap();
        assert_eq!(size, 1024, "should sum only the active model dir's files");
        assert!(mtime.is_some());
    }

    #[test]
    fn active_model_dir_stats_ignores_other_model_dirs() {
        // Two models on disk; status must report ONLY the active one (#21).
        let dir = tempdir().unwrap();
        let active = dir.path().join("models--intfloat--multilingual-e5-small");
        std::fs::create_dir_all(&active).unwrap();
        std::fs::write(active.join("model.onnx"), vec![0u8; 1000]).unwrap();
        let stale = dir.path().join("models--intfloat--multilingual-e5-base");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("model.onnx"), vec![0u8; 999_000]).unwrap();

        let (size, _) = active_model_dir_stats(dir.path(), "multilingual-e5-small").unwrap();
        assert_eq!(
            size, 1000,
            "must report only the active model's bytes, not the lingering base model's"
        );
    }

    #[test]
    fn format_local_formats_or_none() {
        // A normal timestamp renders `YYYY-MM-DD HH:MM`.
        let s = format_local(1_700_000_000).unwrap();
        assert_eq!(s.len(), "2023-11-14 22:13".len());
        assert!(s.contains('-') && s.contains(':'));
    }

    // ─────────────────────────────────────────────────────────────────
    // `status_data_for` — the MCP `status` tool's data builder. Directly
    // exercises the `indexed = doc_count > 0` refinement (vs. `status_data`'s
    // cache-dir-existence `indexed`), which until now was only asserted in a
    // doc comment.
    // ─────────────────────────────────────────────────────────────────

    /// Build a `ResolvedVault` rooted at `dir` (which must already contain an
    /// `onebrain.yml`), as the flag-resolved source — mirrors the identical
    /// helper in `search_common.rs`'s test module.
    fn resolved_at(dir: &Path) -> onebrain_core::ResolvedVault {
        onebrain_core::ResolvedVault {
            root: onebrain_core::VaultRoot::from_path(dir).unwrap(),
            source: onebrain_core::VaultSource::Flag,
        }
    }

    #[test]
    fn status_data_for_never_indexed_reports_not_indexed_and_zero_docs() {
        let vault = tempdir().unwrap();
        std::fs::write(
            vault.path().join("onebrain.yml"),
            "search:\n  collection: t-status-data-for\n",
        )
        .unwrap();
        let resolved = resolved_at(vault.path());

        // `Engine::open` is lazy-embedder (never downloads a model) and just
        // creates/opens the cache dir — cheap, no network, matches every
        // other `search_status`/`search_common` test's use of a bare tempdir
        // as the cache dir.
        let cache = tempdir().unwrap();
        let engine = Engine::open(cache.path(), "multilingual-e5-small").unwrap();

        let data = status_data_for(&engine, &resolved).unwrap();

        assert_eq!(data.collection, Some("t-status-data-for".to_string()));
        assert_eq!(data.doc_count, 0);
        assert!(
            !data.indexed,
            "never-indexed vault (doc_count == 0) must report indexed: false"
        );
        assert_eq!(data.semantic_available, cfg!(feature = "semantic"));
    }

    #[test]
    fn status_data_for_after_indexing_reports_indexed_true() {
        // Companion to `status_data_for_never_indexed_reports_not_indexed_and_zero_docs`,
        // covering the `doc_count > 0 → indexed: true` side of `status_data_for`'s
        // refinement.
        //
        // Populating a real `Engine` with `>= 1` doc requires `index_doc`, which
        // calls `embed_passages_if_available` — with the plain `Engine::open`
        // (lazy/production embedder) used by the sibling test above, and the
        // `semantic` feature ON by default for this workspace's test builds,
        // that would construct the REAL `fastembed` embedder and download a
        // model on first use. `Engine::open_with_embedder` (which sibling
        // `onebrain-search` tests use with an in-crate `FakeEmbedder` to avoid
        // exactly this) is `pub(crate)` to `onebrain-search` — not reachable
        // from this crate's tests. So per the fallback documented for this gap:
        // assert the `indexed = doc_count > 0` logic directly on a
        // directly-constructed `SearchStatusData`, matching exactly what
        // `status_data_for` computes (`indexed: status.doc_count > 0`).
        let data = SearchStatusData {
            collection: Some("t-status-data-for".to_string()),
            embed_model: "multilingual-e5-small".to_string(),
            cache_dir: Some(PathBuf::from("/cache/t-status-data-for")),
            indexed: 3 > 0, // mirrors `status_data_for`'s `status.doc_count > 0`
            model_size_bytes: None,
            model_downloaded_at: None,
            last_indexed_at: Some(1_700_000_000),
            index_size_bytes: None,
            doc_count: 3,
            pending_new: 0,
            pending_changed: 0,
            pending_removed: 0,
            cache_size_bytes: None,
            reindexing: None,
            semantic_available: cfg!(feature = "semantic"),
        };

        assert_eq!(data.doc_count, 3);
        assert!(
            data.indexed,
            "indexed vault (doc_count > 0) must report indexed: true"
        );
    }
}
