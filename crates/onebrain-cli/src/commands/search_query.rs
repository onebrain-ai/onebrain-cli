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
}

/// `onebrain search query` — hybrid (lex + vector, RRF-fused). Opens the
/// full engine; embeds the query text, so this is a model-download point on
/// first use.
pub fn run_query(
    vault_flag: Option<PathBuf>,
    mode: &OutputMode,
    args: &SearchQueryArgs,
) -> Result<()> {
    let (engine, resolved) = open_engine(vault_flag)?;
    let vault_info = crate::vault_ctx::info_from(&resolved);
    let hits = engine.query(&args.text, args.top_k)?;
    emit_hits("search.query", vault_info, hits, mode)
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
    let hits = engine.vector_search(&args.text, args.top_k)?;
    emit_hits("search.vec", vault_info, hits, mode)
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
             💡 set `search.collection` in onebrain.yml (or run `onebrain init`), \
             then run `onebrain search reindex`"
        );
    };
    let cache_dir = collection_cache_dir(&collection);
    let lex = LexIndex::open(&cache_dir.join("tantivy"))
        .with_context(|| format!("opening lex index at {}", cache_dir.display()))?;

    let raw_hits = lex.search(&args.text, args.top_k)?;
    // LexIndex::search returns bare (chunk_id, score) pairs — no doc_path,
    // heading_path, or snippet (those live in the engine's redb metadata,
    // which this verb deliberately never opens). chunk_id already encodes
    // the doc path as its prefix (see `chunk::Chunk::chunk_id`, `<doc_path>#N`),
    // so surface that much; heading_path/snippet are left empty rather than
    // guessed.
    let hits: Vec<HitData> = raw_hits
        .into_iter()
        .map(|(chunk_id, score)| {
            let doc_path = chunk_id
                .rsplit_once('#')
                .map(|(path, _)| path.to_string())
                .unwrap_or_else(|| chunk_id.clone());
            HitData {
                doc_path,
                chunk_id,
                heading_path: String::new(),
                score: score as f64,
                snippet: String::new(),
            }
        })
        .collect();

    let envelope = Envelope::ok("search.lex", Some(vault_info), SearchHitsData { hits });
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

fn emit_hits(
    command: &str,
    vault_info: crate::output::VaultInfo,
    hits: Vec<Hit>,
    mode: &OutputMode,
) -> Result<()> {
    let hits: Vec<HitData> = hits.into_iter().map(HitData::from).collect();
    let envelope = Envelope::ok(command, Some(vault_info), SearchHitsData { hits });
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

fn render_text(env: &Envelope<SearchHitsData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    if d.hits.is_empty() {
        return "🔍 no results".to_string();
    }
    let mut blocks = Vec::with_capacity(d.hits.len());
    for (i, h) in d.hits.iter().enumerate() {
        let rank = i + 1;
        let mut block = if h.heading_path.is_empty() {
            format!("📄 {rank}. {}  ({:.3})", h.doc_path, h.score)
        } else {
            format!(
                "📄 {rank}. {} › {}  ({:.3})",
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
        Envelope::ok("search.query", None, SearchHitsData { hits })
    }

    #[test]
    fn text_handles_no_matches() {
        assert_eq!(render_text(&env(Vec::new())), "🔍 no results");
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
        assert!(s.contains("📄 1. a.md › Intro  (0.500)"));
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
        assert!(s.contains("📄 1. a.md  (0.500)"));
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
        assert!(s.contains("📄 1. a.md"));
        assert!(s.contains("📄 2. b.md"));
        // Blank line separates the two hit blocks.
        assert!(s.contains("first\n\n📄 2."));
    }
}
