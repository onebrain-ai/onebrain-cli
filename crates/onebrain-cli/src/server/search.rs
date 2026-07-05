//! `GET /api/vault/search` — vault search backed by the native
//! `onebrain-search` engine (the SAME index the CLI `onebrain search …`
//! verbs and the native MCP server use).
//!
//! Two modes:
//!
//! ```text
//!   mode=lex     → LexIndex (BM25 keyword, no model, fast as-you-type)
//!   mode=hybrid  → Engine::query (lex + vector, one query embedding)
//!                  (lex-only / --no-default-features builds have no
//!                  embedder, so hybrid degrades to the same LexIndex path
//!                  as mode=lex — see `run_hybrid` below)
//! ```
//!
//! Read-only translator: it returns vault-relative paths the existing
//! `GET /api/vault/file` (with its path-traversal guard) opens; it never
//! reads a note itself and never mutates config. A vault that has never
//! been indexed returns an empty `hits` list (200) — not a 503 — mirroring
//! the native MCP `query` no-index policy.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Query, State},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use super::api::{require_vault_root, ApiError};
use super::{AppState, SharedEngine};
use crate::commands::search_common::{collection_cache_dir, collection_name_readonly};
#[cfg(feature = "semantic")]
use onebrain_search::engine::Engine;
use onebrain_search::lex::LexIndex;

/// Query string for `GET /api/vault/search`.
#[derive(Debug, Deserialize)]
pub(crate) struct SearchQuery {
    /// The user's search text.
    q: String,
    /// `lex` (BM25 keyword, the default) or `hybrid` (keyword + semantic).
    #[serde(default)]
    mode: Option<String>,
}

/// Response body: a ranked hit list plus the mode actually run.
#[derive(Debug, Serialize)]
struct SearchResponse {
    hits: Vec<SearchHit>,
    mode: &'static str,
}

/// One result row for the webui.
#[derive(Debug, Serialize, PartialEq)]
struct SearchHit {
    /// Vault-relative, slash-separated path (openable via `/api/vault/file`).
    path: String,
    /// Relevance score (higher = better). Scale differs by mode.
    score: f64,
    /// Note title — the heading path if present, else the file stem.
    title: String,
    /// Short one-line excerpt (may be empty; lex results carry none).
    snippet: String,
}

/// Max top-k the webui asks for (native search engine caps at ~20; keep parity).
const TOP_K: usize = 20;

/// Hard ceiling on one native search. Lex is ~ms; a cold hybrid embed can
/// take seconds — this sits well above that and only trips on a genuine hang.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) async fn get_vault_search(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SearchQuery>,
) -> Result<Response, ApiError> {
    let root = require_vault_root(&state)?.to_path_buf();
    let query = q.q.trim().to_string();
    let mode: &'static str = match q.mode.as_deref() {
        Some("hybrid") => "hybrid",
        _ => "lex",
    };

    // Empty query → empty result; no work.
    if query.is_empty() {
        return Ok(Json(SearchResponse { hits: vec![], mode }).into_response());
    }

    // Warm-daemon path: when the process holds a persistent engine, route
    // through it instead of opening a fresh one per request. redb is
    // single-writer, so a per-request `Engine::open` in the daemon process
    // would clash with the held engine ("Database already open"); reusing the
    // held handle both avoids that and keeps the boot-time index warm. `serve`
    // / the unit-test router hold no engine and fall through to `run_native`.
    let held = state.search_engine.clone();

    // Native search is synchronous (tantivy / embedding). Run it off the async
    // runtime and bound it so a slow hybrid embed can't wedge a worker.
    let search =
        tokio::task::spawn_blocking(move || run_search(held.as_ref(), &root, &query, mode));
    let hits = match tokio::time::timeout(SEARCH_TIMEOUT, search).await {
        Ok(Ok(Ok(hits))) => hits,
        Ok(Ok(Err(e))) => {
            tracing::warn!(error = %e, "native search failed");
            return Err(ApiError::Internal("search failed".to_string()));
        }
        Ok(Err(join_err)) => {
            tracing::warn!(error = %join_err, "native search task panicked");
            return Err(ApiError::Internal("search failed".to_string()));
        }
        Err(_) => {
            tracing::warn!(
                timeout_s = SEARCH_TIMEOUT.as_secs(),
                "native search timed out"
            );
            return Err(ApiError::Internal("search timed out".to_string()));
        }
    };
    Ok(Json(SearchResponse { hits, mode }).into_response())
}

/// Synchronous search dispatcher, aware of an optionally-held persistent
/// engine (the warm-daemon path). Runs entirely inside `spawn_blocking`.
///
/// - **lex** always uses the standalone [`LexIndex`] (tantivy only — it never
///   opens redb, so it can't clash with a held engine, and needs no lock).
/// - **hybrid** with a held engine reuses that engine under its mutex (the
///   ONLY redb opener in the daemon); without one it falls back to
///   [`run_native`]'s per-request `Engine::open` (the `serve` / CLI path).
fn run_search(
    held: Option<&SharedEngine>,
    root: &Path,
    query: &str,
    mode: &str,
) -> anyhow::Result<Vec<SearchHit>> {
    // No held engine → the per-request path, unchanged (`serve` / CLI).
    let Some(engine) = held else {
        return run_native(root, query, mode);
    };

    let collection = collection_name_readonly(root)?;
    let cache_dir = collection_cache_dir(&collection);

    // Never-indexed vault → empty hits (200), not an error — mirrors
    // `run_native`'s short-circuit so a held engine on an empty index behaves
    // identically to the per-request path.
    if !cache_dir.join("tantivy").exists() {
        return Ok(vec![]);
    }

    // lex never opens redb (tantivy only), so it stays on the standalone index
    // even with a held engine — no lock, no contention. Only hybrid reuses the
    // held (sole redb-owning) engine.
    if mode == "hybrid" {
        run_hybrid_held(engine, &cache_dir, root, query)
    } else {
        run_lex(&cache_dir, query)
    }
}

/// Hybrid search against the daemon's held engine (the sole redb owner). Locks
/// the engine for the query, mirroring [`run_hybrid`]'s empty-index guard +
/// hit-mapping so results are identical to the per-request path.
///
/// Semantic build only: a lex-only build has no embedder and degrades hybrid to
/// lex (there is no held engine to query differently), so the held path is
/// gated to `semantic` and the lex-only variant below routes to [`run_lex`].
#[cfg(feature = "semantic")]
fn run_hybrid_held(
    engine: &SharedEngine,
    _cache_dir: &Path,
    root: &Path,
    query: &str,
) -> anyhow::Result<Vec<SearchHit>> {
    let engine = engine.lock().unwrap_or_else(|p| p.into_inner());
    if engine.status(root)?.doc_count == 0 {
        return Ok(vec![]);
    }
    Ok(engine
        .query(query, TOP_K)?
        .into_iter()
        .filter(|h| !h.doc_path.is_empty())
        .map(|h| SearchHit {
            title: if h.heading_path.is_empty() {
                title_from_path(&h.doc_path)
            } else {
                h.heading_path.clone()
            },
            path: h.doc_path,
            score: h.score,
            snippet: h.snippet,
        })
        .collect())
}

/// Lex-only build: a held engine can't do vector search (no embedder), so
/// hybrid degrades to keyword ranking. `cache_dir` is re-derived by the caller;
/// route straight to [`run_lex`] via the standalone index. `root`/`engine` are
/// unused here — the held engine offers nothing a lex-only build can use.
#[cfg(not(feature = "semantic"))]
fn run_hybrid_held(
    _engine: &SharedEngine,
    cache_dir: &Path,
    _root: &Path,
    query: &str,
) -> anyhow::Result<Vec<SearchHit>> {
    run_lex(cache_dir, query)
}

/// Synchronous native search. `mode` is `"hybrid"` or `"lex"`.
fn run_native(root: &Path, query: &str, mode: &str) -> anyhow::Result<Vec<SearchHit>> {
    let collection = collection_name_readonly(root)?;
    let cache_dir = collection_cache_dir(&collection);

    // Never-indexed vault → empty hits (200), not an error. The lex path
    // would create an empty index and return nothing anyway; short-circuit
    // so hybrid never opens the engine / embeds against an empty index.
    if !cache_dir.join("tantivy").exists() {
        return Ok(vec![]);
    }

    if mode == "hybrid" {
        run_hybrid(&cache_dir, root, query)
    } else {
        run_lex(&cache_dir, query)
    }
}

/// Lex (BM25) via `LexIndex` — no engine, no embedder, no model download.
/// `LexIndex::search` returns bare `(chunk_id, score)`; `chunk_id` prefixes
/// the doc path (`<doc_path>#N`), so surface that as the path + title, with
/// no snippet (the snippet lives in engine metadata this path never opens).
fn run_lex(cache_dir: &Path, query: &str) -> anyhow::Result<Vec<SearchHit>> {
    let lex = LexIndex::open(&cache_dir.join("tantivy"))?;
    let raw = lex.search(query, TOP_K)?;
    Ok(raw
        .into_iter()
        .filter_map(|(chunk_id, score)| {
            let doc_path = chunk_id
                .rsplit_once('#')
                .map(|(p, _)| p.to_string())
                .unwrap_or(chunk_id);
            if doc_path.is_empty() {
                return None;
            }
            Some(SearchHit {
                title: title_from_path(&doc_path),
                path: doc_path,
                score: f64::from(score),
                snippet: String::new(),
            })
        })
        .collect())
}

/// Hybrid (lex + vector) via the engine. Guarded on `doc_count == 0` so an
/// empty index never triggers a query embedding (and thus never a model
/// download) — it returns empty hits instead.
///
/// Semantic build only: in a `--no-default-features` (lex-only) build there
/// is no embedder, so `Engine::query` would `bail!` on a non-empty index
/// (see `onebrain_search::engine::Engine::embedder`) — see the
/// `#[cfg(not(feature = "semantic"))]` variant below, which degrades to
/// [`run_lex`] instead, mirroring `commands::search_query::run_query`.
#[cfg(feature = "semantic")]
fn run_hybrid(cache_dir: &Path, root: &Path, query: &str) -> anyhow::Result<Vec<SearchHit>> {
    let config = onebrain_core::load_vault_config_at(root)?;
    let engine = Engine::open(cache_dir, &config.search.embed_model)?;
    if engine.status(root)?.doc_count == 0 {
        return Ok(vec![]);
    }
    Ok(engine
        .query(query, TOP_K)?
        .into_iter()
        .filter(|h| !h.doc_path.is_empty())
        .map(|h| SearchHit {
            title: if h.heading_path.is_empty() {
                title_from_path(&h.doc_path)
            } else {
                h.heading_path.clone()
            },
            path: h.doc_path,
            score: h.score,
            snippet: h.snippet,
        })
        .collect())
}

/// Lex-only build: hybrid degrades to keyword (BM25) ranking via [`run_lex`]
/// rather than calling `Engine::query`, which has no embedder to fall back
/// on in this build and would error instead of degrading. `root` is unused
/// here (only needed to load `search.embed_model` for the real engine).
#[cfg(not(feature = "semantic"))]
fn run_hybrid(cache_dir: &Path, _root: &Path, query: &str) -> anyhow::Result<Vec<SearchHit>> {
    run_lex(cache_dir, query)
}

/// File stem of a slash-separated vault path (`a/b/note.md` → `note`), used
/// as a fallback title when there is no heading.
fn title_from_path(path: &str) -> String {
    let file = path.rsplit('/').next().unwrap_or(path);
    file.strip_suffix(".md").unwrap_or(file).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_from_path_strips_dirs_and_md_suffix() {
        assert_eq!(title_from_path("01-projects/oma/x.md"), "x");
        assert_eq!(title_from_path("note.md"), "note");
        assert_eq!(title_from_path("no-extension"), "no-extension");
    }

    #[test]
    fn run_native_no_index_returns_empty() {
        // A vault dir with a config but no built index → empty hits, no error,
        // for both modes. Pointing `ONEBRAIN_CACHE_DIR` at a fresh, empty
        // tempdir guarantees `<cache>/search/never-indexed/tantivy` genuinely
        // doesn't exist. `test_env` holds the crate-wide env lock for the
        // guard's lifetime (see its module doc — a module-private lock here
        // still raced `session_init.rs`'s tests on the same variable).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "search:\n  collection: never-indexed\n",
        )
        .unwrap();
        let cache = tempfile::tempdir().unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
        let result_lex = run_native(dir.path(), "anything", "lex");
        let result_hybrid = run_native(dir.path(), "anything", "hybrid");
        assert!(result_lex.unwrap().is_empty());
        assert!(result_hybrid.unwrap().is_empty());
    }

    #[test]
    fn run_lex_returns_hits_from_a_prebuilt_index() {
        use onebrain_search::chunk::Chunk;
        let cache = tempfile::tempdir().unwrap();
        let tantivy_dir = cache.path().join("tantivy");
        {
            let mut lex = LexIndex::open(&tantivy_dir).unwrap();
            lex.add(&Chunk {
                chunk_id: "notes/alpha.md#0".to_string(),
                doc_path: "notes/alpha.md".to_string(),
                heading_path: String::new(),
                text: "the quick brown fox jumps".to_string(),
                chunk_index: 0,
            })
            .unwrap();
            lex.commit().unwrap();
        }
        let hits = run_lex(cache.path(), "quick fox").unwrap();
        assert_eq!(hits.len(), 1, "hits: {hits:?}");
        assert_eq!(hits[0].path, "notes/alpha.md");
        assert_eq!(hits[0].title, "alpha");
        assert!(hits[0].snippet.is_empty());
    }

    /// Lex-only builds have no embedder — `Engine::query` would `bail!` on a
    /// non-empty index (see `onebrain_search::engine::Engine::embedder`).
    /// `run_native` must route `mode=hybrid` to the lex path instead of
    /// calling the engine, so this never errors even against real hits.
    /// Gated to lex-only: under the `semantic` feature, hitting the hybrid
    /// path here would construct a real embedder / attempt a model download.
    #[cfg(not(feature = "semantic"))]
    #[test]
    fn run_native_hybrid_degrades_to_lex_in_lex_only_build() {
        use onebrain_search::chunk::Chunk;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("onebrain.yml"),
            "search:\n  collection: hybrid-degrade-test\n",
        )
        .unwrap();
        let cache = tempfile::tempdir().unwrap();
        let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());

        let collection = collection_name_readonly(dir.path()).unwrap();
        let cache_dir = collection_cache_dir(&collection);
        {
            let mut lex = LexIndex::open(&cache_dir.join("tantivy")).unwrap();
            lex.add(&Chunk {
                chunk_id: "notes/alpha.md#0".to_string(),
                doc_path: "notes/alpha.md".to_string(),
                heading_path: String::new(),
                text: "the quick brown fox jumps".to_string(),
                chunk_index: 0,
            })
            .unwrap();
            lex.commit().unwrap();
        }

        let result = run_native(dir.path(), "quick fox", "hybrid");

        let hits = result.expect("hybrid must degrade to lex, not error, in a lex-only build");
        assert_eq!(hits.len(), 1, "hits: {hits:?}");
        assert_eq!(hits[0].path, "notes/alpha.md");
    }
}
