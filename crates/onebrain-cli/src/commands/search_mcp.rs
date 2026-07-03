//! `onebrain search mcp` — MCP stdio server over the native search engine.
//!
//! Mirrors the qmd MCP tool surface (`query`/`get`/`multi_get`/`status`) so the
//! plugin's `.mcp.json` can swap `qmd mcp` -> `onebrain search mcp` without any
//! instruction changes (tool namespace rename lands in v3.4.2). tokio lives only
//! at this boundary; the sync engine is called via `spawn_blocking`.
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::transport::stdio;
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler, ServiceExt};

use onebrain_core::path::ResolvedVault;
use onebrain_search::engine::{Engine, Hit, SEMANTIC_UNAVAILABLE};
use onebrain_search::lex::LexIndex;

use super::search_common::{collection_cache_dir, collection_for, open_engine};
use super::search_status::{status_data_for, SearchStatusData};

#[derive(Clone)]
pub struct SearchMcpServer {
    engine: Arc<Mutex<Engine>>,
    resolved: ResolvedVault,
    tool_router: ToolRouter<Self>,
}

fn internal(e: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

/// A single typed sub-query in a `query` tool call.
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SubQuery {
    /// lex = BM25 keywords; vec = semantic question; hyde = hypothetical answer passage (embedded like vec).
    pub r#type: SubQueryType,
    pub query: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SubQueryType {
    Lex,
    Vec,
    Hyde,
}

/// Params for the `query` tool. Mirrors the qmd MCP tool surface — several
/// fields (`candidateLimit`, `collections`, `intent`, `rerank`) are accepted
/// for compatibility but not yet used by the native engine (see field docs).
/// `#[allow(dead_code)]`: these are deserialize-only — populated by callers'
/// JSON payloads (schema compatibility), not read by Rust code yet.
#[derive(serde::Deserialize, schemars::JsonSchema)]
#[allow(dead_code)]
pub struct QueryParams {
    /// Typed sub-queries to execute (1-10). The first gets 2x weight in fusion.
    pub searches: Vec<SubQuery>,
    /// Max results (default 10).
    pub limit: Option<usize>,
    /// Min normalized relevance 0-1 (default 0). Top fused hit scores 1.0.
    #[serde(rename = "minScore")]
    pub min_score: Option<f64>,
    /// Accepted for qmd compatibility; not used by the native engine.
    #[serde(rename = "candidateLimit")]
    pub candidate_limit: Option<usize>,
    /// Accepted for qmd compatibility; the native index is single-collection per vault.
    pub collections: Option<Vec<String>>,
    /// Background context to disambiguate. Accepted for compatibility; not yet used in ranking (relevance phase, v3.4.3).
    pub intent: Option<String>,
    /// Accepted for qmd compatibility; native rerank lands in v3.4.3.
    pub rerank: Option<bool>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct QueryOut {
    pub results: Vec<QueryHit>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct QueryHit {
    pub docid: String, // chunk id
    pub file: String,  // vault-relative doc path
    pub title: String, // file stem
    pub score: f64,    // normalized 0-1 (top fused hit = 1.0)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>, // heading path, if any
    pub snippet: String,
}

const RRF_K: f64 = 60.0;

/// Client-side RRF fusion: each `(weight, hits)` pair is a ranked sub-query
/// result list (already truncated to its own over-fetch limit). Fuses by
/// `chunk_id`, accumulating `weight / (RRF_K + rank + 1)` across lists,
/// then sorts descending and normalizes so the top hit scores exactly 1.0.
fn rrf_fuse(ranked: Vec<(f64, Vec<Hit>)>) -> Vec<(f64, Hit)> {
    use std::collections::HashMap;
    let mut acc: HashMap<String, (f64, Hit)> = HashMap::new();
    for (weight, hits) in ranked {
        for (rank, hit) in hits.into_iter().enumerate() {
            let s = weight / (RRF_K + rank as f64 + 1.0);
            acc.entry(hit.chunk_id.clone())
                .and_modify(|(total, _)| *total += s)
                .or_insert((s, hit));
        }
    }
    let mut out: Vec<_> = acc.into_values().collect();
    out.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.chunk_id.cmp(&b.1.chunk_id))
    });
    if let Some(max) = out.first().map(|(s, _)| *s).filter(|s| *s > 0.0) {
        for (s, _) in &mut out {
            *s /= max;
        }
    }
    out
}

/// Run a single lex sub-query. Reuses `run_lex`'s exact engine call
/// (`commands/search_query.rs::run_lex`): open `LexIndex` directly at the
/// collection's `tantivy/` dir (bypassing the embedder entirely), so a lex
/// sub-query never triggers a model download. Returned hits carry only
/// `chunk_id`/`doc_path`/`score` — `heading_path`/`snippet` are left empty,
/// same as `run_lex`'s output (see its comment on why: that metadata lives in
/// the engine's redb, which this path deliberately never opens).
fn lex_subquery(resolved: &ResolvedVault, text: &str, top_k: usize) -> anyhow::Result<Vec<Hit>> {
    let collection = collection_for(resolved)?;
    let cache_dir = collection_cache_dir(&collection);
    let lex = LexIndex::open(&cache_dir.join("tantivy"))
        .with_context(|| format!("opening lex index at {}", cache_dir.display()))?;
    let raw_hits = lex.search(text, top_k)?;
    Ok(raw_hits
        .into_iter()
        .map(|(chunk_id, score)| {
            let doc_path = chunk_id
                .rsplit_once('#')
                .map(|(path, _)| path.to_string())
                .unwrap_or_else(|| chunk_id.clone());
            Hit {
                doc_path,
                chunk_id,
                heading_path: String::new(),
                score: score as f64,
                snippet: String::new(),
            }
        })
        .collect())
}

/// File stem of a vault-relative doc path, for `QueryHit::title`.
fn title_from_doc_path(doc_path: &str) -> String {
    std::path::Path::new(doc_path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| doc_path.to_string())
}

impl From<Hit> for QueryHit {
    fn from(h: Hit) -> Self {
        Self {
            title: title_from_doc_path(&h.doc_path),
            file: h.doc_path,
            docid: h.chunk_id,
            score: h.score,
            context: (!h.heading_path.is_empty()).then_some(h.heading_path),
            snippet: h.snippet,
        }
    }
}

#[tool_router]
impl SearchMcpServer {
    pub fn new(engine: Engine, resolved: ResolvedVault) -> Self {
        Self {
            engine: Arc::new(Mutex::new(engine)),
            resolved,
            tool_router: Self::tool_router(),
        }
    }

    /// Runs a closure against the engine on a blocking thread.
    async fn with_engine<T, F>(&self, f: F) -> Result<T, ErrorData>
    where
        T: Send + 'static,
        F: FnOnce(&mut Engine) -> anyhow::Result<T> + Send + 'static,
    {
        let engine = self.engine.clone();
        tokio::task::spawn_blocking(move || {
            let mut eng = engine
                .lock()
                .map_err(|_| anyhow::anyhow!("engine mutex poisoned"))?;
            f(&mut eng)
        })
        .await
        .map_err(internal)?
        .map_err(internal)
    }

    #[tool(
        name = "status",
        description = "Show the status of the search index: collection, embed model, document counts, pending changes, and health information."
    )]
    async fn status(&self) -> Result<Json<SearchStatusData>, ErrorData> {
        let resolved = self.resolved.clone();
        self.with_engine(move |eng| status_data_for(eng, &resolved))
            .await
            .map(Json)
    }

    #[tool(
        name = "query",
        description = "Search the vault with typed sub-queries (lex = BM25 keywords, vec = semantic question, hyde = hypothetical answer passage), fused client-side via RRF. The first sub-query gets 2x weight."
    )]
    async fn query(
        &self,
        Parameters(params): Parameters<QueryParams>,
    ) -> Result<Json<QueryOut>, ErrorData> {
        if params.searches.is_empty() || params.searches.len() > 10 {
            return Err(ErrorData::invalid_params(
                "searches must contain 1-10 sub-queries",
                None,
            ));
        }

        let limit = params.limit.unwrap_or(10);
        let min_score = params.min_score.unwrap_or(0.0);
        let fetch_k = limit.max(10) * 3;
        let has_lex = params
            .searches
            .iter()
            .any(|s| matches!(s.r#type, SubQueryType::Lex));

        let resolved = self.resolved.clone();
        let ranked: Vec<(f64, Vec<Hit>)> = self
            .with_engine(move |eng| {
                let mut ranked = Vec::with_capacity(params.searches.len());
                for (i, sub) in params.searches.into_iter().enumerate() {
                    let weight = if i == 0 { 2.0 } else { 1.0 };
                    let hits = match sub.r#type {
                        SubQueryType::Lex => lex_subquery(&resolved, &sub.query, fetch_k)?,
                        SubQueryType::Vec | SubQueryType::Hyde => {
                            match eng.vector_search(&sub.query, fetch_k) {
                                Ok(hits) => hits,
                                Err(e) if has_lex && e.to_string() == SEMANTIC_UNAVAILABLE => {
                                    // Degrade: skip this sub-query, same as
                                    // `run_query`'s hybrid-to-lex-only degradation,
                                    // since at least one lex sub-query is present.
                                    Vec::new()
                                }
                                Err(e) => return Err(e),
                            }
                        }
                    };
                    ranked.push((weight, hits));
                }
                Ok(ranked)
            })
            .await?;

        let fused = rrf_fuse(ranked);
        let results: Vec<QueryHit> = fused
            .into_iter()
            .filter(|(score, _)| *score >= min_score)
            .take(limit)
            .map(|(score, mut hit)| {
                hit.score = score;
                QueryHit::from(hit)
            })
            .collect();

        Ok(Json(QueryOut { results }))
    }
}

#[tool_handler(router = self.tool_router.clone())]
impl ServerHandler for SearchMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(
                "OneBrain native vault search. Use `query` with typed sub-queries \
                 (lex = BM25 keywords, vec = semantic question, hyde = hypothetical \
                 answer passage); `get`/`multi_get` to read documents; `status` for \
                 index health. Paths are vault-relative.",
            )
            .with_server_info(Implementation::new("onebrain", env!("CARGO_PKG_VERSION")))
    }
}

pub fn run(vault_flag: Option<PathBuf>) -> Result<()> {
    let (engine, resolved) = open_engine(vault_flag)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime for search mcp")?;
    runtime.block_on(async move {
        let service = SearchMcpServer::new(engine, resolved)
            .serve(stdio())
            .await
            .context("initialize MCP stdio server")?;
        service.waiting().await.context("MCP server terminated")?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use onebrain_search::engine::Hit;

    fn hit(id: &str) -> Hit {
        Hit {
            chunk_id: id.into(),
            doc_path: format!("{id}.md"),
            heading_path: String::new(),
            score: 0.0,
            snippet: String::new(),
        }
    }

    #[test]
    fn rrf_fuse_first_subquery_double_weight_wins_ties() {
        // "a" is rank-0 in the first (2x) list; "b" is rank-0 in the second.
        let fused = rrf_fuse(vec![(2.0, vec![hit("a")]), (1.0, vec![hit("b")])]);
        assert_eq!(fused[0].1.chunk_id, "a");
        assert!(
            (fused[0].0 - 1.0).abs() < 1e-9,
            "top hit must normalize to 1.0"
        );
        assert!(fused[1].0 < 1.0 && fused[1].0 > 0.0);
    }

    #[test]
    fn rrf_fuse_dedupes_by_chunk_id_accumulating_score() {
        // x: weight 2.0 at rank 0 (2/61) + weight 1.0 at rank 1 (1/62) = 0.048921
        // y: weight 1.0 at rank 0 (1/61)                              = 0.016393
        // normalized: x = 1.0, y = 0.016393 / 0.048921 = 0.33510
        // A last-write-wins bug would instead leave x = 1/62 and flip the ranking.
        let fused = rrf_fuse(vec![(2.0, vec![hit("x")]), (1.0, vec![hit("y"), hit("x")])]);
        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].1.chunk_id, "x");
        assert!((fused[0].0 - 1.0).abs() < 1e-9);
        assert!(
            (fused[1].0 - 0.33510).abs() < 1e-4,
            "y normalized score was {}",
            fused[1].0
        );
    }

    #[test]
    fn rrf_fuse_empty_input_is_empty() {
        assert!(rrf_fuse(vec![]).is_empty());
    }

    #[test]
    fn rrf_fuse_breaks_score_ties_by_chunk_id() {
        // Two hits that only ever appear at the same weight and rank in
        // separate lists → identical RRF scores; order must be deterministic.
        let fused = rrf_fuse(vec![(1.0, vec![hit("b")]), (1.0, vec![hit("a")])]);
        assert_eq!(fused[0].1.chunk_id, "a");
        assert_eq!(fused[1].1.chunk_id, "b");
    }
}
