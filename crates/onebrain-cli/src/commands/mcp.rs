//! `onebrain mcp` — OneBrain's MCP stdio server.
//!
//! Search tools (`query`/`get`/`multi_get`/`status`) are the first tool group
//! hosted here; more vault tool groups (notes, tasks, ...) will mount on this
//! same command over time. The current tool surface mirrors the qmd MCP tools
//! so the plugin's `.mcp.json` can swap `qmd mcp` -> `onebrain mcp` without any
//! instruction changes (tool namespace rename lands in v3.4.2). tokio lives only
//! at this boundary; the sync engine is called via `spawn_blocking`.
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::transport::stdio;
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler, ServiceExt};

use onebrain_core::path::ResolvedVault;
use onebrain_search::engine::{Engine, Hit};
use onebrain_search::lex::LexIndex;

use super::search_common::{collection_cache_dir, collection_for, open_engine};
use super::search_status::{status_data_for, SearchStatusData};

#[derive(Clone)]
pub struct McpServer {
    engine: Arc<Mutex<Engine>>,
    resolved: ResolvedVault,
    tool_router: ToolRouter<Self>,
}

fn internal(e: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

/// Default per-file byte cap for `multi_get` — files larger than this are
/// skipped (with a one-line note in the output) rather than dumped whole.
const DEFAULT_MAX_BYTES: u64 = 10_240;

/// Splits a trailing `:N` line-number suffix off a path, e.g.
/// `notes/a.md:100` -> (`notes/a.md`, `Some(100)`). Only a purely-numeric
/// suffix counts as a line number — this keeps Windows drive letters
/// (`a:b.md`) and any other non-numeric `:`-suffix intact as part of the
/// path instead of misparsing them.
fn split_line_suffix(input: &str) -> (&str, Option<usize>) {
    if let Some((path, suffix)) = input.rsplit_once(':') {
        if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
            return (path, suffix.parse().ok());
        }
    }
    (input, None)
}

/// Resolves a vault-relative path to an absolute, canonicalized path that is
/// guaranteed to live under `vault_root` — the traversal guard for `get` /
/// `multi_get`. Canonicalizing both sides (not just comparing the joined
/// path textually) means `..` segments AND symlinks that point outside the
/// vault are both caught: `starts_with` runs against the fully resolved
/// target, so a symlink inside the vault pointing at `/etc/passwd` resolves
/// to `/etc/passwd` before the check and is rejected exactly like a literal
/// `../etc/passwd` would be.
fn resolve_under_vault(vault_root: &Path, rel: &str) -> anyhow::Result<PathBuf> {
    let root = vault_root
        .canonicalize()
        .context("canonicalize vault root")?;
    // Absolute `rel` inputs (e.g. `/etc/passwd`) are rejected too: `Path::join`
    // with an absolute path discards `root` entirely and returns the absolute
    // path unchanged, so the `starts_with(&root)` check below still catches it
    // (the canonicalized absolute path won't start with the vault root) — but
    // only for paths outside the vault; an absolute path that happens to fall
    // *inside* the vault would incorrectly pass, which is why callers should
    // always pass vault-relative paths, not attacker-controlled absolute ones.
    let joined = root.join(rel);
    let canon = joined
        .canonicalize()
        .with_context(|| format!("not found: {rel}"))?;
    anyhow::ensure!(canon.starts_with(&root), "path escapes the vault: {rel}");
    Ok(canon)
}

/// Slices `text` to the `[from_line, from_line + max_lines)` window
/// (1-indexed, inclusive start), optionally prefixing each line with its
/// 1-indexed line number.
fn slice_lines(
    text: &str,
    from_line: Option<usize>,
    max_lines: Option<usize>,
    line_numbers: bool,
) -> String {
    let start = from_line.unwrap_or(1).max(1);
    let lines = text.lines().enumerate().skip(start - 1);
    let lines: Box<dyn Iterator<Item = (usize, &str)>> = match max_lines {
        Some(n) => Box::new(lines.take(n)),
        None => Box::new(lines),
    };
    let mut out: Vec<String> = Vec::new();
    for (i, l) in lines {
        out.push(if line_numbers {
            format!("{}: {}", i + 1, l)
        } else {
            l.to_string()
        });
    }
    out.join("\n")
}

/// Params for the `get` tool.
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct GetParams {
    /// Vault-relative file path from search results (e.g. 'notes/meeting.md', or 'notes/meeting.md:100' to start at line 100).
    pub file: String,
    /// Start from this line number (1-indexed).
    #[serde(rename = "fromLine")]
    pub from_line: Option<usize>,
    /// Maximum number of lines to return.
    #[serde(rename = "maxLines")]
    pub max_lines: Option<usize>,
    /// Add line numbers ('N: content').
    #[serde(rename = "lineNumbers")]
    pub line_numbers: Option<bool>,
}

/// Params for the `multi_get` tool.
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct MultiGetParams {
    /// Glob pattern (vault-relative, e.g. 'journals/2026-07*.md') or comma-separated list of paths.
    pub pattern: String,
    #[serde(rename = "maxLines")]
    pub max_lines: Option<usize>,
    /// Skip files larger than this many bytes (default 10240).
    #[serde(rename = "maxBytes")]
    pub max_bytes: Option<u64>,
    #[serde(rename = "lineNumbers")]
    pub line_numbers: Option<bool>,
}

/// Vault-relative paths (forward-slash, POSIX-style) matched by `pattern`
/// under `vault_root`. A `,`-containing pattern is treated as an explicit
/// comma-separated path list (trimmed, no globbing); otherwise `pattern` is
/// compiled as a glob and matched against every file reachable by walking
/// `vault_root` (hidden dirs like `.git` are skipped, matching the search
/// engine's own indexing walk).
fn expand_multi_get_pattern(vault_root: &Path, pattern: &str) -> anyhow::Result<Vec<String>> {
    if pattern.contains(',') {
        return Ok(pattern
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect());
    }

    let glob = globset::Glob::new(pattern)
        .with_context(|| format!("invalid glob pattern: {pattern}"))?
        .compile_matcher();

    let mut matched = Vec::new();
    for entry in walkdir::WalkDir::new(vault_root)
        .into_iter()
        .filter_entry(|e| {
            // Skip dotfiles/dotdirs (e.g. `.git`, `.obsidian`) — same
            // convention as the search engine's own indexing walk.
            e.file_name()
                .to_str()
                .map(|name| e.depth() == 0 || !name.starts_with('.'))
                .unwrap_or(true)
        })
    {
        let entry = entry.context("walking vault tree")?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(vault_root)
            .unwrap_or(entry.path());
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if glob.is_match(&rel_str) {
            matched.push(rel_str);
        }
    }
    matched.sort();
    Ok(matched)
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
/// The genuinely inert fields (deserialize-only, never read by Rust code yet)
/// carry their own `#[allow(dead_code)]` so a FUTURE field that's actually
/// unused would still trip clippy instead of hiding behind a blanket
/// struct-level allow.
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct QueryParams {
    /// Typed sub-queries to execute (1-10). The first gets 2x weight in fusion.
    pub searches: Vec<SubQuery>,
    /// Max results (default 10).
    pub limit: Option<usize>,
    /// Min normalized relevance 0-1 (default 0). Top fused hit scores 1.0.
    #[serde(rename = "minScore")]
    pub min_score: Option<f64>,
    /// Accepted for qmd compatibility; not used by the native engine.
    #[allow(dead_code)]
    #[serde(rename = "candidateLimit")]
    pub candidate_limit: Option<usize>,
    /// Accepted for qmd compatibility; the native index is single-collection per vault.
    #[allow(dead_code)]
    pub collections: Option<Vec<String>>,
    /// Background context to disambiguate. Accepted for compatibility; not yet used in ranking (relevance phase, v3.4.3).
    #[allow(dead_code)]
    pub intent: Option<String>,
    /// Accepted for qmd compatibility; native rerank lands in v3.4.3.
    #[allow(dead_code)]
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
///
/// NOTE: this is intentionally a DIFFERENT formula from the engine-internal
/// `onebrain_search::hybrid::rrf_fuse` — do NOT "unify" them. This one is a
/// weighted (first sub-query ×2), multi-sub-query fusion normalized to 1.0 for
/// qmd-compatible `query`-tool output; the engine's is the fixed two-list
/// (lex + vec) hybrid fusion. They serve different callers and must stay
/// separate.
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

/// Degradation policy for a vec/hyde sub-query's `vector_search` result inside
/// the `query` tool. When a lex sub-query is present (`has_lex`), ANY error
/// from the vector side (embedder-unavailable in a lex-only build, OR a
/// mid-query model-download failure in a semantic build) is swallowed and the
/// sub-query degrades to empty hits — the lex sub-query still answers, mirroring
/// `run_query`'s hybrid-to-lex-only degradation. The error is logged to stderr
/// (never stdout — this is a JSON-RPC stdio server; stdout carries the protocol
/// frame). When there is NO lex sub-query, the error propagates: an all-vec
/// query with no embedding capability must error, matching `run_vsearch`
/// (vector-only has no lex analogue, so it errors instead of degrading).
///
/// Split out of the tool body so the degradation branch is unit-testable
/// without a running engine. Note: guarding on the exact `SEMANTIC_UNAVAILABLE`
/// string would be dead code in the shipped (semantic-on) build, where that
/// string never occurs — hence the `has_lex`-only guard here.
fn degrade_vec_error(has_lex: bool, result: anyhow::Result<Vec<Hit>>) -> anyhow::Result<Vec<Hit>> {
    match result {
        Ok(hits) => Ok(hits),
        Err(e) if has_lex => {
            eprintln!("onebrain mcp: vec/hyde sub-query degraded to lex (skipping): {e:#}");
            Ok(Vec::new())
        }
        Err(e) => Err(e),
    }
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
impl McpServer {
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
            // Recover the guard on poison (a prior closure panicked) instead of
            // erroring forever — matches the rest of the codebase
            // (`update.rs`'s `.lock().unwrap_or_else(|e| e.into_inner())`) so a
            // single panicking tool call doesn't brick every subsequent
            // engine-backed call for the server's lifetime.
            let mut eng = engine.lock().unwrap_or_else(|e| e.into_inner());
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
                            degrade_vec_error(has_lex, eng.vector_search(&sub.query, fetch_k))?
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

    #[tool(
        name = "get",
        description = "Read a file's contents by vault-relative path (from search results). Supports 'path:N' to start at line N, and fromLine/maxLines/lineNumbers for windowing. Out-of-range line numbers are clamped rather than erroring: fromLine 0 is treated as line 1, and a fromLine past the end of the file returns an empty result."
    )]
    async fn get(
        &self,
        Parameters(params): Parameters<GetParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let (path_part, suffix_line) = split_line_suffix(&params.file);
        let path_part = path_part.to_string();
        let vault_root = self.resolved.root.as_path().to_path_buf();

        let resolved_path =
            tokio::task::spawn_blocking(move || resolve_under_vault(&vault_root, &path_part))
                .await
                .map_err(internal)?
                .map_err(|e| ErrorData::invalid_params(e.to_string(), None))?;

        let text = tokio::fs::read_to_string(&resolved_path)
            .await
            .map_err(|e| {
                ErrorData::invalid_params(format!("reading {}: {e}", params.file), None)
            })?;

        let from_line = suffix_line.or(params.from_line);
        let sliced = slice_lines(
            &text,
            from_line,
            params.max_lines,
            params.line_numbers.unwrap_or(false),
        );
        Ok(CallToolResult::success(vec![ContentBlock::text(sliced)]))
    }

    #[tool(
        name = "multi_get",
        description = "Read multiple files at once by glob pattern (e.g. 'journals/2026-07*.md') or a comma-separated list of vault-relative paths. Files larger than maxBytes (default 10240) are skipped with a note. Glob walks exclude hidden directories (dot-dirs, e.g. `.git`/`.obsidian`), mirroring the search engine's own indexing convention."
    )]
    async fn multi_get(
        &self,
        Parameters(params): Parameters<MultiGetParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let vault_root = self.resolved.root.as_path().to_path_buf();
        let max_bytes = params.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
        let line_numbers = params.line_numbers.unwrap_or(false);
        let max_lines = params.max_lines;
        let pattern = params.pattern.clone();

        let paths = {
            let vault_root = vault_root.clone();
            tokio::task::spawn_blocking(move || expand_multi_get_pattern(&vault_root, &pattern))
                .await
                .map_err(internal)?
                .map_err(|e| ErrorData::invalid_params(e.to_string(), None))?
        };

        if paths.is_empty() {
            return Ok(CallToolResult::success(vec![ContentBlock::text(
                "no files matched",
            )]));
        }

        let mut sections: Vec<String> = Vec::with_capacity(paths.len());
        for rel in paths {
            let vault_root = vault_root.clone();
            let rel_for_resolve = rel.clone();
            let resolved_path = match tokio::task::spawn_blocking(move || {
                resolve_under_vault(&vault_root, &rel_for_resolve)
            })
            .await
            .map_err(internal)?
            {
                Ok(p) => p,
                Err(e) => {
                    sections.push(format!("--- {rel}\n(skipped: {e})"));
                    continue;
                }
            };

            let metadata = match tokio::fs::metadata(&resolved_path).await {
                Ok(m) => m,
                Err(e) => {
                    sections.push(format!("--- {rel}\n(skipped: {e})"));
                    continue;
                }
            };
            if metadata.len() > max_bytes {
                sections.push(format!(
                    "--- {rel}\n(skipped: {} bytes exceeds maxBytes {max_bytes})",
                    metadata.len()
                ));
                continue;
            }

            match tokio::fs::read_to_string(&resolved_path).await {
                Ok(text) => {
                    let sliced = slice_lines(&text, None, max_lines, line_numbers);
                    sections.push(format!("--- {rel}\n{sliced}"));
                }
                Err(e) => sections.push(format!("--- {rel}\n(skipped: {e})")),
            }
        }

        Ok(CallToolResult::success(vec![ContentBlock::text(
            sections.join("\n\n"),
        )]))
    }
}

#[tool_handler(router = self.tool_router.clone())]
impl ServerHandler for McpServer {
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
        .context("build tokio runtime for onebrain mcp")?;
    runtime.block_on(async move {
        let service = McpServer::new(engine, resolved)
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

    // ─────────────────────────────────────────────────────────────────
    // Wire-contract tests: deserialize from raw JSON using the camelCase
    // wire keys an actual MCP client sends. These exist because the
    // `#[serde(rename = "...")]`/`rename_all` attributes above are never
    // exercised through real JSON in the other tests — a typo in a rename
    // (e.g. `"fromline"` instead of `"fromLine"`) would silently pass every
    // other test in this module while breaking every real client call.
    // ─────────────────────────────────────────────────────────────────

    #[test]
    fn get_params_deserialize_camelcase_wire_keys() {
        let p: GetParams = serde_json::from_value(serde_json::json!({
            "file": "a.md:2", "fromLine": 5, "maxLines": 3, "lineNumbers": true
        }))
        .unwrap();
        assert_eq!(p.file, "a.md:2");
        assert_eq!(p.from_line, Some(5));
        assert_eq!(p.max_lines, Some(3));
        assert_eq!(p.line_numbers, Some(true));
    }

    #[test]
    fn multi_get_params_deserialize_camelcase_wire_keys() {
        let p: MultiGetParams = serde_json::from_value(serde_json::json!({
            "pattern": "notes/*.md", "maxLines": 3, "maxBytes": 2048, "lineNumbers": true
        }))
        .unwrap();
        assert_eq!(p.pattern, "notes/*.md");
        assert_eq!(p.max_lines, Some(3));
        assert_eq!(p.max_bytes, Some(2048));
        assert_eq!(p.line_numbers, Some(true));
    }

    #[test]
    fn query_params_deserialize_camelcase_wire_keys() {
        // Exercise all three `SubQueryType` variants — a typo in the
        // `rename_all = "lowercase"` attribute would silently break the vec/hyde
        // sub-queries a real client sends while a lex-only test still passed.
        let p: QueryParams = serde_json::from_value(serde_json::json!({
            "searches": [
                {"type": "lex", "query": "foo"},
                {"type": "vec", "query": "bar"},
                {"type": "hyde", "query": "baz"}
            ],
            "minScore": 0.5,
            "candidateLimit": 100
        }))
        .unwrap();
        assert_eq!(p.searches.len(), 3);
        assert!(matches!(p.searches[0].r#type, SubQueryType::Lex));
        assert_eq!(p.searches[0].query, "foo");
        assert!(matches!(p.searches[1].r#type, SubQueryType::Vec));
        assert_eq!(p.searches[1].query, "bar");
        assert!(matches!(p.searches[2].r#type, SubQueryType::Hyde));
        assert_eq!(p.searches[2].query, "baz");
        assert_eq!(p.min_score, Some(0.5));
        assert_eq!(p.candidate_limit, Some(100));
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

    #[test]
    fn split_line_suffix_parses_colon_line() {
        assert_eq!(
            split_line_suffix("notes/a.md:100"),
            ("notes/a.md", Some(100))
        );
        assert_eq!(split_line_suffix("notes/a.md"), ("notes/a.md", None));
        // Windows drive letters / non-numeric suffixes are not line numbers.
        assert_eq!(split_line_suffix("a:b.md"), ("a:b.md", None));
    }

    #[test]
    fn slice_lines_respects_from_max_and_numbers() {
        let text = "l1\nl2\nl3\nl4";
        assert_eq!(slice_lines(text, Some(2), Some(2), false), "l2\nl3");
        assert_eq!(
            slice_lines(text, None, None, true),
            "1: l1\n2: l2\n3: l3\n4: l4"
        );
    }

    #[test]
    fn resolve_under_vault_rejects_escape() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ok.md"), "x").unwrap();
        assert!(resolve_under_vault(dir.path(), "ok.md").is_ok());
        assert!(resolve_under_vault(dir.path(), "../etc/passwd").is_err());
        // Absolute inputs are rejected too — not just relative traversal.
        assert!(resolve_under_vault(dir.path(), "/etc/passwd").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn resolve_under_vault_rejects_symlink_escape() {
        // A symlink that lives inside the vault but points outside it must
        // be rejected too — canonicalize() follows the symlink before the
        // `starts_with` check runs, so the escape is caught the same way a
        // literal `../` would be.
        let vault = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.md"), "SECRET").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.md"),
            vault.path().join("link.md"),
        )
        .unwrap();

        assert!(resolve_under_vault(vault.path(), "link.md").is_err());
    }

    // ─────────────────────────────────────────────────────────────────
    // `degrade_vec_error` — the query tool's vec/hyde degradation policy.
    // ─────────────────────────────────────────────────────────────────

    #[test]
    fn degrade_vec_error_with_lex_swallows_error_returns_empty() {
        // has_lex=true + any vec/hyde error → degrade to empty hits (the lex
        // sub-query still answers), NOT a propagated error. This is the branch
        // that was previously dead in the shipped semantic build.
        let err: anyhow::Result<Vec<Hit>> = Err(anyhow::anyhow!("model download failed mid-query"));
        let out = degrade_vec_error(true, err).expect("should degrade, not error");
        assert!(out.is_empty(), "degraded sub-query must yield empty hits");
    }

    #[test]
    fn degrade_vec_error_with_lex_passes_through_ok_hits() {
        let ok: anyhow::Result<Vec<Hit>> = Ok(vec![hit("a")]);
        let out = degrade_vec_error(true, ok).expect("ok result must pass through");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].chunk_id, "a");
    }

    #[test]
    fn degrade_vec_error_without_lex_propagates_error() {
        // No lex sub-query to fall back to → an all-vec query with no embedding
        // capability must error (matches `run_vsearch`).
        let err: anyhow::Result<Vec<Hit>> = Err(anyhow::anyhow!("no ONNX runtime"));
        match degrade_vec_error(false, err) {
            Ok(_) => panic!("no-lex vec error must propagate, not degrade"),
            Err(e) => assert!(e.to_string().contains("no ONNX runtime")),
        }
    }

    // ─────────────────────────────────────────────────────────────────
    // `expand_multi_get_pattern` — comma-list parsing + glob walking.
    // ─────────────────────────────────────────────────────────────────

    #[test]
    fn expand_multi_get_pattern_splits_and_trims_comma_list() {
        // A comma in the pattern → explicit path list, trimmed, no globbing;
        // the vault_root is irrelevant for the comma-list branch.
        let dir = tempfile::tempdir().unwrap();
        let out =
            expand_multi_get_pattern(dir.path(), " notes/a.md , notes/b.md ,notes/c.md ").unwrap();
        assert_eq!(out, vec!["notes/a.md", "notes/b.md", "notes/c.md"]);
    }

    #[test]
    fn expand_multi_get_pattern_drops_empty_comma_segments() {
        let dir = tempfile::tempdir().unwrap();
        let out = expand_multi_get_pattern(dir.path(), "a.md,, ,b.md").unwrap();
        assert_eq!(out, vec!["a.md", "b.md"]);
    }

    #[test]
    fn expand_multi_get_pattern_glob_matches_multiple_files_sorted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("2026-07-01.md"), "x").unwrap();
        std::fs::write(dir.path().join("2026-07-02.md"), "y").unwrap();
        std::fs::write(dir.path().join("2026-08-01.md"), "z").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "t").unwrap();
        let out = expand_multi_get_pattern(dir.path(), "2026-07*.md").unwrap();
        assert_eq!(out, vec!["2026-07-01.md", "2026-07-02.md"]);
    }

    #[test]
    fn expand_multi_get_pattern_glob_matches_nested_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("journals")).unwrap();
        std::fs::write(dir.path().join("journals/day.md"), "x").unwrap();
        std::fs::write(dir.path().join("top.md"), "y").unwrap();
        let out = expand_multi_get_pattern(dir.path(), "**/*.md").unwrap();
        // Forward-slash, POSIX-style rel paths on every platform.
        assert!(out.contains(&"journals/day.md".to_string()));
        assert!(out.contains(&"top.md".to_string()));
    }

    #[test]
    fn expand_multi_get_pattern_glob_excludes_hidden_dirs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::create_dir_all(dir.path().join(".obsidian")).unwrap();
        std::fs::write(dir.path().join(".git/config.md"), "x").unwrap();
        std::fs::write(dir.path().join(".obsidian/app.md"), "y").unwrap();
        std::fs::write(dir.path().join("real.md"), "z").unwrap();
        let out = expand_multi_get_pattern(dir.path(), "**/*.md").unwrap();
        assert_eq!(
            out,
            vec!["real.md"],
            "dot-dirs must be excluded from glob walk"
        );
    }

    #[test]
    fn expand_multi_get_pattern_glob_no_match_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.md"), "z").unwrap();
        let out = expand_multi_get_pattern(dir.path(), "nomatch-*.md").unwrap();
        assert!(out.is_empty(), "no-match glob must return empty vec");
    }
}
