//! `onebrain search query` / `search search` / `search vsearch` — hybrid,
//! lex-only, and vector-only search over the native index.
//!
//! `run_lex` is the ONLY one of the three that must never trigger a model
//! download: it opens the `LexIndex` directly (bypassing `Engine::open`'s
//! embedder entirely — see `onebrain_search::lex::LexIndex`), so `search
//! search` stays fast even before any embedding model has been fetched.
//! `run_query` (hybrid) and `run_vsearch` (vector-only) both call into the
//! `Engine`, whose embedder now lazy-inits on first use (engine.rs) — so
//! the download, when it happens, happens here, not at `Engine::open`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::cli::SearchQueryArgs;
use crate::commands::search_common::{collection_cache_dir, open_engine, resolve_collection};
use crate::output::{emit, Envelope, OutputMode};
use onebrain_search::engine::Hit;
use onebrain_search::lex::LexIndex;

#[derive(Debug, Serialize)]
struct HitData {
    chunk_id: String,
    doc_path: String,
    heading_path: String,
    score: f64,
    /// On the lex `search` verb an empty `snippet` means "not supported on this
    /// verb yet" (deferred — the tantivy `body` field is indexed but NOT
    /// STORED, so there's no text to slice a snippet from without a schema
    /// change + reindex migration). It does NOT mean "no matching text". The
    /// hybrid `query`/`vsearch` verbs DO populate it from redb-stored chunk
    /// text. See v3.4.6 bug E.
    snippet: String,
}

impl From<Hit> for HitData {
    fn from(h: Hit) -> Self {
        Self {
            chunk_id: h.chunk_id,
            doc_path: h.doc_path,
            heading_path: h.heading_path,
            score: h.score,
            snippet: h.snippet,
        }
    }
}

#[derive(Debug, Serialize)]
struct SearchHitsData {
    hits: Vec<HitData>,
    /// Present only when `hits` is empty AND the index state explains it
    /// (empty or stale index) — so "no results" never silently means
    /// "you forgot to reindex".
    #[serde(skip_serializing_if = "Option::is_none")]
    index_hint: Option<String>,
}

/// Why an empty result might not mean "no matching notes": an empty or
/// stale index. Best-effort — a status probe failure degrades to no hint
/// rather than failing the search.
fn index_hint_for(
    engine: &onebrain_search::engine::Engine,
    resolved: &onebrain_core::ResolvedVault,
) -> Option<String> {
    let st = engine.status(resolved.root.as_path()).ok()?;
    if st.doc_count == 0 {
        return Some("index is empty — run `onebrain search reindex` first".to_string());
    }
    let pending = st.pending_total();
    (pending > 0).then(|| {
        format!(
            "index is behind — {pending} doc(s) not yet indexed · run `onebrain search reindex`"
        )
    })
}

/// `onebrain search query` — hybrid (lex + vector, RRF-fused). Opens the
/// full engine; embeds the query text, so this is a model-download point on
/// first use.
///
/// In a lex-only build (no `semantic` feature) there is no embedder, so hybrid
/// isn't possible — this degrades to lex-only (BM25) ranking, printing a
/// one-line notice on stderr so the result isn't silently narrower than the
/// user expects. `vsearch` (vector-only) has no lex analogue, so it errors
/// instead of degrading.
#[cfg(feature = "semantic")]
pub fn run_query(
    vault_flag: Option<PathBuf>,
    mode: &OutputMode,
    args: &SearchQueryArgs,
) -> Result<()> {
    let (engine, resolved) = open_engine(vault_flag)?;
    let vault_info = crate::vault_ctx::info_from(&resolved);
    let hits = apply_min_score(engine.query(&args.text, args.top_k)?, args.min_score);
    let hint = hits
        .is_empty()
        .then(|| index_hint_for(&engine, &resolved))
        .flatten();
    emit_hits("search.query", vault_info, hits, hint, mode)
}

/// Lex-only build: hybrid degrades to keyword (BM25) ranking with a stderr
/// notice. Delegates to [`run_lex`] so the exact same lex path/output is used.
#[cfg(not(feature = "semantic"))]
pub fn run_query(
    vault_flag: Option<PathBuf>,
    mode: &OutputMode,
    args: &SearchQueryArgs,
) -> Result<()> {
    eprintln!("ℹ️  semantic search unavailable in this build — showing keyword (lex-only) results");
    run_lex(vault_flag, mode, args)
}

/// `onebrain search vsearch` — vector-only semantic search. Also embeds the
/// query text (model-download point on first use).
pub fn run_vsearch(
    vault_flag: Option<PathBuf>,
    mode: &OutputMode,
    args: &SearchQueryArgs,
) -> Result<()> {
    let (engine, resolved) = open_engine(vault_flag)?;
    let vault_info = crate::vault_ctx::info_from(&resolved);
    let hits = apply_min_score(
        engine.vector_search(&args.text, args.top_k)?,
        args.min_score,
    );
    let hint = hits
        .is_empty()
        .then(|| index_hint_for(&engine, &resolved))
        .flatten();
    emit_hits("search.vec", vault_info, hits, hint, mode)
}

/// `onebrain search search` — lex-only (BM25) search. Deliberately does NOT
/// go through `open_engine`/`Engine`: it opens `LexIndex` directly so this
/// verb never constructs an embedder and never downloads a model, even on
/// first run.
pub fn run_lex(
    vault_flag: Option<PathBuf>,
    mode: &OutputMode,
    args: &SearchQueryArgs,
) -> Result<()> {
    let (resolved, collection) = resolve_collection(vault_flag)?;
    let vault_info = crate::vault_ctx::info_from(&resolved);

    let Some(collection) = collection else {
        anyhow::bail!(
            "❌ no search collection configured\n\
             💡  set `search.collection` in onebrain.yml (or run `onebrain init`), \
             then run `onebrain search reindex`"
        );
    };
    let cache_dir = collection_cache_dir(&collection);
    let lex = LexIndex::open(&cache_dir.join("tantivy"))
        .with_context(|| format!("opening lex index at {}", cache_dir.display()))?;

    let mut raw_hits = lex.search_with_heading(&args.text, args.top_k)?;
    if let Some(min) = args.min_score {
        raw_hits.retain(|(_, _, score)| f64::from(*score) >= min);
    }
    // `search_with_heading` returns (chunk_id, heading_path, score). The
    // heading_path IS a STORED tantivy field, so we populate it here WITHOUT
    // opening the engine's redb metadata (v3.4.6, bug E). chunk_id encodes the
    // doc path as its prefix (see `chunk::Chunk::chunk_id`, `<doc_path>#N`), so
    // derive doc_path from it. The `snippet` stays empty: the chunk text lives
    // in the `body` field, which is indexed but NOT `STORED` in tantivy — a
    // snippet would need a schema change + full reindex migration, deferred to
    // a follow-up (bug E is cosmetic/low-priority).
    let hits: Vec<HitData> = raw_hits
        .into_iter()
        .map(|(chunk_id, heading_path, score)| {
            let doc_path = chunk_id
                .rsplit_once('#')
                .map(|(path, _)| path.to_string())
                .unwrap_or_else(|| chunk_id.clone());
            HitData {
                doc_path,
                chunk_id,
                heading_path,
                score: score as f64,
                snippet: String::new(),
            }
        })
        .collect();

    // No engine here (deliberately — no embedder), so no index-state probe.
    let envelope = Envelope::ok(
        "search.lex",
        Some(vault_info),
        SearchHitsData {
            hits,
            index_hint: None,
        },
    );
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

/// Apply the optional `--min-score` floor to engine hits.
fn apply_min_score(mut hits: Vec<Hit>, min_score: Option<f64>) -> Vec<Hit> {
    if let Some(min) = min_score {
        hits.retain(|h| h.score >= min);
    }
    hits
}

fn emit_hits(
    command: &str,
    vault_info: crate::output::VaultInfo,
    hits: Vec<Hit>,
    index_hint: Option<String>,
    mode: &OutputMode,
) -> Result<()> {
    let hits: Vec<HitData> = hits.into_iter().map(HitData::from).collect();
    let envelope = Envelope::ok(
        command,
        Some(vault_info),
        SearchHitsData { hits, index_hint },
    );
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

fn render_text(env: &Envelope<SearchHitsData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    if d.hits.is_empty() {
        return match &d.index_hint {
            Some(h) => format!("🔍  No results\nℹ️  {h}"),
            None => "🔍  No results".to_string(),
        };
    }
    let mut blocks = Vec::with_capacity(d.hits.len());
    for (i, h) in d.hits.iter().enumerate() {
        let rank = i + 1;
        let mut block = if h.heading_path.is_empty() {
            format!("📄  {rank}. {}  ({:.3})", h.doc_path, h.score)
        } else {
            format!(
                "📄  {rank}. {} › {}  ({:.3})",
                h.doc_path, h.heading_path, h.score
            )
        };
        if !h.snippet.is_empty() {
            block.push_str(&format!("\n     {}", h.snippet));
        }
        blocks.push(block);
    }
    // Blank line between hits so each result reads as its own block.
    blocks.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(hits: Vec<HitData>) -> Envelope<SearchHitsData> {
        Envelope::ok(
            "search.query",
            None,
            SearchHitsData {
                hits,
                index_hint: None,
            },
        )
    }

    #[test]
    fn text_handles_no_matches() {
        assert_eq!(render_text(&env(Vec::new())), "🔍  No results");
    }

    #[test]
    fn text_surfaces_index_hint_on_no_matches() {
        let e = Envelope::ok(
            "search.query",
            None,
            SearchHitsData {
                hits: Vec::new(),
                index_hint: Some("index is empty — run `onebrain search reindex` first".into()),
            },
        );
        let s = render_text(&e);
        assert!(s.contains("🔍  No results"), "{s}");
        assert!(s.contains("index is empty"), "{s}");
        assert!(s.contains("search reindex"), "{s}");
    }

    #[test]
    fn text_renders_hit_with_heading_and_snippet() {
        let s = render_text(&env(vec![HitData {
            chunk_id: "a.md#0".into(),
            doc_path: "a.md".into(),
            heading_path: "Intro".into(),
            score: 0.5,
            snippet: "hello world".into(),
        }]));
        assert!(s.contains("📄  1. a.md › Intro  (0.500)"));
        assert!(s.contains("hello world"));
    }

    #[test]
    fn text_renders_hit_without_heading() {
        let s = render_text(&env(vec![HitData {
            chunk_id: "a.md#0".into(),
            doc_path: "a.md".into(),
            heading_path: String::new(),
            score: 0.5,
            snippet: String::new(),
        }]));
        assert!(s.contains("📄  1. a.md  (0.500)"));
        assert!(!s.contains("›"));
    }

    #[test]
    fn text_ranks_and_separates_multiple_hits() {
        let s = render_text(&env(vec![
            HitData {
                chunk_id: "a.md#0".into(),
                doc_path: "a.md".into(),
                heading_path: String::new(),
                score: 0.9,
                snippet: "first".into(),
            },
            HitData {
                chunk_id: "b.md#0".into(),
                doc_path: "b.md".into(),
                heading_path: String::new(),
                score: 0.5,
                snippet: "second".into(),
            },
        ]));
        assert!(s.contains("📄  1. a.md"));
        assert!(s.contains("📄  2. b.md"));
        // Blank line separates the two hit blocks.
        assert!(s.contains("first\n\n📄  2."));
    }
}
