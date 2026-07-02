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
pub fn run_query(
    vault_flag: Option<PathBuf>,
    mode: &OutputMode,
    args: &SearchQueryArgs,
) -> Result<()> {
    let (engine, resolved) = open_engine(vault_flag)?;
    let vault_info = crate::vault_ctx::info_from(&resolved);
    let hits = engine.query(&args.text, args.top_k)?;
    let hint = hits
        .is_empty()
        .then(|| index_hint_for(&engine, &resolved))
        .flatten();
    emit_hits("search.query", vault_info, hits, hint, mode)
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
            Some(h) => format!("🔍 no results\nℹ️  {h}"),
            None => "🔍 no results".to_string(),
        };
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
        assert_eq!(render_text(&env(Vec::new())), "🔍 no results");
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
        assert!(s.contains("🔍 no results"), "{s}");
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
