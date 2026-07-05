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

use super::daemon_client::{self, DaemonHandle};
use super::search_common::{collection_cache_dir, collection_for, open_engine};
use super::search_status::{
    status_data_for, status_data_from_daemon, DaemonStatusCounts, SearchStatusData,
};

/// How the MCP server reaches the search engine.
///
/// redb is single-process, so the engine must have exactly ONE owner. The
/// primary path is [`Backend::Daemon`]: the warm daemon owns the engine and the
/// MCP server routes `status`/`query` through it over HTTP, holding no engine
/// itself — this is what lets multiple concurrent MCP sessions coexist without
/// racing redb's single-writer lock. [`Backend::Direct`] is the fallback for a
/// lone session where the daemon couldn't be started: the server opens the
/// engine itself (today's behaviour), which still works precisely because it's
/// the only owner in that case.
#[derive(Clone)]
enum Backend {
    /// The daemon owns the engine; we talk to it over HTTP. No redb lock held.
    Daemon(DaemonHandle),
    /// No daemon available — this process opened the engine directly (the sole
    /// owner). Holds the redb lock for its lifetime, so only viable solo.
    Direct(Arc<Mutex<Engine>>),
}

#[derive(Clone)]
pub struct McpServer {
    backend: Backend,
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
    /// Set when there's no search index yet (e.g. never reindexed) instead of
    /// erroring — lets the calling agent degrade to filesystem search.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
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

/// The `tantivy/` lexical index dir exists under a collection cache dir.
/// Pure fs — the cheapest "is there an index to search?" probe.
fn tantivy_index_present(cache_dir: &Path) -> bool {
    cache_dir.join("tantivy").is_dir()
}

/// Agent-facing note returned by `query` when the vault has no index yet, so
/// the caller degrades to filesystem search instead of treating it as an error.
fn no_index_note() -> String {
    "No search index for this vault yet — run `onebrain search reindex`, or fall back to filesystem search (grep/Read)."
        .to_string()
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

/// The daemon `mode` string for a typed sub-query. The daemon's
/// `GET /api/vault/search` speaks only `lex` (BM25) or `hybrid` (lex + vector),
/// so a `vec`/`hyde` sub-query maps to `hybrid` (which embeds the query and
/// fuses with lex inside the daemon), and `lex` maps to `lex`.
fn daemon_mode_for(t: &SubQueryType) -> &'static str {
    match t {
        SubQueryType::Lex => "lex",
        SubQueryType::Vec | SubQueryType::Hyde => "hybrid",
    }
}

/// Parse the daemon's `GET /api/vault/search` response body into `Hit`s — the
/// WIRE CONTRACT between `server::search::SearchResponse`/`SearchHit` (producer)
/// and this consumer. The daemon emits
/// `{ "hits": [ { "path", "score", "title", "snippet" }, .. ], "mode": ".." }`.
///
/// The daemon's hits are DOC-level (no chunk id — that lives in the engine's
/// redb, which the daemon owns and doesn't expose), so `chunk_id` is set to the
/// `path`: client-side RRF fusion then dedupes by document path across
/// sub-queries. `heading_path` carries the daemon's `title` when it's a real
/// heading path; the file stem (`title == stem`) is dropped so `QueryHit::from`
/// re-derives the stem itself and `context` stays `None` for stem-only titles,
/// matching the direct-engine path where lex hits carry an empty heading.
fn parse_daemon_search_hits(body: &serde_json::Value) -> anyhow::Result<Vec<Hit>> {
    let hits = body
        .get("hits")
        .and_then(|h| h.as_array())
        .ok_or_else(|| anyhow::anyhow!("daemon search response missing `hits` array"))?;
    let mut out = Vec::with_capacity(hits.len());
    for h in hits {
        let path = h
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("daemon search hit missing `path`"))?
            .to_string();
        let score = h.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let title = h.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let snippet = h
            .get("snippet")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // The daemon sets `title` to the heading path when present, else the
        // file stem. Only keep it as `heading_path` when it's NOT just the
        // stem, so stem-only hits round-trip to an empty heading (context=None)
        // exactly like the direct lex path.
        let heading_path = if title.is_empty() || title == title_from_doc_path(&path) {
            String::new()
        } else {
            title.to_string()
        };
        out.push(Hit {
            chunk_id: path.clone(),
            doc_path: path,
            heading_path,
            score,
            snippet,
        });
    }
    Ok(out)
}

/// Parse the daemon's `GET /api/internal/status` response body into
/// [`DaemonStatusCounts`] — the WIRE CONTRACT between
/// `server::internal::InternalStatusResponse` (producer) and this consumer. The
/// daemon emits `{ "doc_count", "pending_new", "pending_changed",
/// "pending_removed", "pending_total", "last_indexed", "indexed" }`. Only the
/// engine-truth counts are consumed here; the client re-derives `indexed` /
/// `pending_total` itself, and pairs these with local filesystem reads to build
/// the full `SearchStatusData`.
fn parse_daemon_status(body: &serde_json::Value) -> anyhow::Result<DaemonStatusCounts> {
    let u = |key: &str| -> anyhow::Result<usize> {
        body.get(key)
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .ok_or_else(|| anyhow::anyhow!("daemon status missing numeric `{key}`"))
    };
    Ok(DaemonStatusCounts {
        doc_count: u("doc_count")?,
        pending_new: u("pending_new")?,
        pending_changed: u("pending_changed")?,
        pending_removed: u("pending_removed")?,
        // `last_indexed` is `null` when never indexed — absent/null → None,
        // a number → Some. A non-numeric non-null value is a contract break.
        last_indexed: match body.get("last_indexed") {
            None | Some(serde_json::Value::Null) => None,
            Some(v) => Some(
                v.as_u64()
                    .ok_or_else(|| anyhow::anyhow!("daemon status `last_indexed` not a number"))?,
            ),
        },
    })
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
    /// Construct a daemon-backed server: the daemon owns the engine, so this
    /// process holds none and can coexist with other MCP/CLI clients.
    fn with_daemon(handle: DaemonHandle, resolved: ResolvedVault) -> Self {
        Self {
            backend: Backend::Daemon(handle),
            resolved,
            tool_router: Self::tool_router(),
        }
    }

    /// Construct a direct-engine server (fallback): this process owns the engine
    /// and holds the redb lock for its lifetime. Only safe as a lone owner.
    fn with_direct_engine(engine: Engine, resolved: ResolvedVault) -> Self {
        Self {
            backend: Backend::Direct(Arc::new(Mutex::new(engine))),
            resolved,
            tool_router: Self::tool_router(),
        }
    }

    /// Runs a closure against the directly-owned engine on a blocking thread.
    /// Only valid on the [`Backend::Direct`] fallback path — the daemon path
    /// owns no engine and routes over HTTP instead.
    async fn with_engine<T, F>(&self, f: F) -> Result<T, ErrorData>
    where
        T: Send + 'static,
        F: FnOnce(&mut Engine) -> anyhow::Result<T> + Send + 'static,
    {
        let Backend::Direct(engine) = &self.backend else {
            return Err(internal(
                "with_engine called on a daemon-backed server (no local engine)",
            ));
        };
        let engine = engine.clone();
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
        match &self.backend {
            // Daemon path: the daemon owns the engine and returns the live
            // counts over HTTP; we pair them with local filesystem reads to
            // build the identical `SearchStatusData` shape.
            Backend::Daemon(handle) => {
                let handle = handle.clone();
                let data = tokio::task::spawn_blocking(move || {
                    let body = handle.status()?;
                    let counts = parse_daemon_status(&body)?;
                    status_data_from_daemon(&resolved, counts)
                })
                .await
                .map_err(internal)?
                .map_err(internal)?;
                Ok(Json(data))
            }
            // Fallback path: read status off the locally-owned engine.
            Backend::Direct(_) => self
                .with_engine(move |eng| status_data_for(eng, &resolved))
                .await
                .map(Json),
        }
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

        let collection = collection_for(&self.resolved).map_err(internal)?;
        let cache_dir = collection_cache_dir(&collection);
        if !tantivy_index_present(&cache_dir) {
            return Ok(Json(QueryOut {
                results: vec![],
                note: Some(no_index_note()),
            }));
        }

        let ranked: Vec<(f64, Vec<Hit>)> = match &self.backend {
            // Daemon path: route EACH sub-query through the daemon's held engine
            // over HTTP (`GET /api/vault/search`) — lex → `lex`, vec/hyde →
            // `hybrid`. The MCP server opens no engine, so N concurrent MCP
            // sessions all fan out to the one daemon with no redb contention.
            Backend::Daemon(handle) => {
                let handle = handle.clone();
                tokio::task::spawn_blocking(move || {
                    let mut ranked = Vec::with_capacity(params.searches.len());
                    for (i, sub) in params.searches.into_iter().enumerate() {
                        let weight = if i == 0 { 2.0 } else { 1.0 };
                        let mode = daemon_mode_for(&sub.r#type);
                        // Mirror the direct path's degradation: a vec/hyde
                        // sub-query error degrades to empty when a lex sub-query
                        // can still answer; a lone vec/hyde error propagates.
                        let hits = degrade_vec_error(
                            has_lex && !matches!(sub.r#type, SubQueryType::Lex),
                            handle
                                .search(&sub.query, mode)
                                .and_then(|body| parse_daemon_search_hits(&body)),
                        )?;
                        ranked.push((weight, hits));
                    }
                    Ok::<_, anyhow::Error>(ranked)
                })
                .await
                .map_err(internal)?
                .map_err(internal)?
            }
            // Fallback path: run sub-queries against the locally-owned engine
            // (lex bypasses redb via a standalone LexIndex; vec/hyde use it).
            Backend::Direct(_) => {
                let resolved = self.resolved.clone();
                self.with_engine(move |eng| {
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
                .await?
            }
        };

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

        Ok(Json(QueryOut {
            results,
            note: None,
        }))
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
    // Resolve the vault FIRST — fail fast with the vault-not-found guard (exit
    // 64) before touching the daemon, exactly as the pre-daemon `open_engine`
    // first-statement did. This keeps `onebrain mcp` outside a vault a clean
    // early exit (no daemon spawned) and gives both backends a resolved vault
    // for `get`/`multi_get` filesystem reads.
    let resolved = crate::vault_ctx::require(vault_flag.clone())?;

    // The daemon `ensure_running` may spawn resolves ITS vault from
    // `$ONEBRAIN_VAULT` (its detached child chdir's to `/`, so walk-up is
    // useless — see `daemon::resolve_daemon_vault`). Export the vault we just
    // resolved so a daemon we auto-start holds the engine for the SAME vault —
    // otherwise it would bind vault-less and `/api/internal/status` /
    // `/api/vault/search` would 503/empty. In production the launcher already
    // sets this; bridging it here makes `onebrain --vault <path> mcp` work too.
    // Only set it when unset, so an explicit `$ONEBRAIN_VAULT` still wins.
    if std::env::var_os("ONEBRAIN_VAULT").is_none() {
        std::env::set_var("ONEBRAIN_VAULT", resolved.root.as_path());
    }

    // Primary path: route through the warm daemon so the MCP server holds NO
    // redb lock and multiple concurrent MCP sessions coexist. `ensure_running`
    // auto-starts the daemon (the sole engine owner) if it isn't up yet.
    //
    // Fallback: if the daemon genuinely can't start (e.g. spawn failed), open
    // the engine directly so a lone session still works — today's behaviour.
    // This is the ONLY safe direct-open: as the sole owner there's no lock race.
    let server = match daemon_client::ensure_running() {
        Ok(handle) => McpServer::with_daemon(handle, resolved),
        Err(daemon_err) => {
            tracing::warn!(
                error = %daemon_err,
                "warm daemon unavailable; falling back to a direct single-process engine open"
            );
            let (engine, resolved) = open_engine(vault_flag).with_context(|| {
                format!("daemon unavailable ({daemon_err:#}) and direct engine open also failed")
            })?;
            McpServer::with_direct_engine(engine, resolved)
        }
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime for onebrain mcp")?;
    runtime.block_on(async move {
        let service = server
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

    // ─────────────────────────────────────────────────────────────────
    // No-index fallback signal for the `query` tool.
    // ─────────────────────────────────────────────────────────────────

    #[test]
    fn no_index_note_mentions_reindex_and_fallback() {
        let n = no_index_note();
        assert!(n.contains("onebrain search reindex"), "{n}");
        assert!(
            n.to_lowercase().contains("fall back") || n.to_lowercase().contains("filesystem"),
            "{n}"
        );
    }

    #[test]
    fn index_present_false_when_tantivy_dir_absent() {
        let dir = tempfile::tempdir().unwrap();
        // A cache dir with no `tantivy/` subdir → no index.
        assert!(!tantivy_index_present(dir.path()));
        std::fs::create_dir_all(dir.path().join("tantivy")).unwrap();
        assert!(tantivy_index_present(dir.path()));
    }

    // ─────────────────────────────────────────────────────────────────
    // Wire-contract: the EXACT JSON bodies the daemon's `server::search` /
    // `server::internal` routes emit must parse cleanly into what the MCP
    // client consumes. Nothing pinned this until a consumer wired in (round-3
    // review) — these are that pin. A field/shape drift on either side
    // (rename, type change, dropped key) breaks these instead of silently
    // returning empty results to a live client.
    // ─────────────────────────────────────────────────────────────────

    #[test]
    fn daemon_mode_maps_subquery_types() {
        assert_eq!(daemon_mode_for(&SubQueryType::Lex), "lex");
        assert_eq!(daemon_mode_for(&SubQueryType::Vec), "hybrid");
        assert_eq!(daemon_mode_for(&SubQueryType::Hyde), "hybrid");
    }

    #[test]
    fn parse_daemon_search_hits_matches_server_wire_shape() {
        // The exact shape `server::search::SearchResponse` serializes:
        // { "hits": [ { path, score, title, snippet } ], "mode" }.
        // The daemon sets `title` to the file STEM when there's no heading
        // (`server::search::title_from_path`) — for `notes/a.md` that's `"a"`.
        let body = serde_json::json!({
            "hits": [
                {"path": "notes/a.md", "score": 3.5, "title": "a", "snippet": ""},
                {"path": "notes/b.md", "score": 1.2, "title": "A Heading > Sub", "snippet": "…text…"}
            ],
            "mode": "lex"
        });
        let hits = parse_daemon_search_hits(&body).unwrap();
        assert_eq!(hits.len(), 2);
        // Stem-equal title → empty heading (context stays None, like direct lex).
        assert_eq!(hits[0].doc_path, "notes/a.md");
        assert_eq!(hits[0].chunk_id, "notes/a.md", "chunk_id defaults to path");
        assert_eq!(hits[0].score, 3.5);
        assert!(hits[0].heading_path.is_empty());
        // A real heading path (not the stem) is preserved as heading_path.
        assert_eq!(hits[1].heading_path, "A Heading > Sub");
        assert_eq!(hits[1].snippet, "…text…");

        // Round-trip into a QueryHit: stem-only → context None; heading → Some.
        // (Hit isn't Clone; consume the vec in order.)
        let mut it = hits.into_iter();
        let qh0 = QueryHit::from(it.next().unwrap());
        let qh1 = QueryHit::from(it.next().unwrap());
        assert!(qh0.context.is_none());
        assert_eq!(qh1.context.as_deref(), Some("A Heading > Sub"));
    }

    #[test]
    fn parse_daemon_search_hits_rejects_missing_hits_array() {
        // A body without `hits` is a contract break, NOT an empty result — the
        // client must error rather than silently degrade to "no matches".
        let body = serde_json::json!({ "mode": "lex" });
        assert!(parse_daemon_search_hits(&body).is_err());
    }

    #[test]
    fn parse_daemon_status_matches_internal_wire_shape() {
        // The exact shape `server::internal::InternalStatusResponse` emits.
        let body = serde_json::json!({
            "doc_count": 7,
            "pending_new": 1,
            "pending_changed": 2,
            "pending_removed": 3,
            "pending_total": 6,
            "last_indexed": 1_720_000_000_u64,
            "indexed": true
        });
        let c = parse_daemon_status(&body).unwrap();
        assert_eq!(c.doc_count, 7);
        assert_eq!(c.pending_new, 1);
        assert_eq!(c.pending_changed, 2);
        assert_eq!(c.pending_removed, 3);
        assert_eq!(c.last_indexed, Some(1_720_000_000));
    }

    #[test]
    fn parse_daemon_status_null_last_indexed_is_none() {
        // A never-indexed daemon reports `last_indexed: null` → None (distinct
        // from a real epoch), and a missing key is likewise None.
        let with_null = serde_json::json!({
            "doc_count": 0, "pending_new": 0, "pending_changed": 0,
            "pending_removed": 0, "pending_total": 0, "last_indexed": null, "indexed": false
        });
        assert_eq!(parse_daemon_status(&with_null).unwrap().last_indexed, None);
    }

    #[test]
    fn parse_daemon_status_rejects_missing_count() {
        // A dropped count key breaks the contract — must error, not default to 0.
        let body = serde_json::json!({ "pending_new": 0 });
        assert!(parse_daemon_status(&body).is_err());
    }

    // ─────────────────────────────────────────────────────────────────
    // Multi-session coexistence + fallback: end-to-end against a REAL
    // ephemeral daemon-style server holding ONE engine over a lex-seeded
    // vault. Download-free (lex). This proves the core win — N MCP clients
    // fan out to ONE engine owner with no redb "Database already open" — and
    // exercises the direct-open fallback path with no daemon at all.
    // ─────────────────────────────────────────────────────────────────

    #[cfg(test)]
    mod daemon_backed {
        use super::*;
        use crate::commands::daemon_client::{DaemonHandle, DaemonInfo};
        use crate::commands::search_common::collection_cache_dir;
        use onebrain_search::chunk::Chunk;
        use onebrain_search::lex::LexIndex;
        use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
        use std::sync::Arc;
        use std::time::{Duration, Instant};
        use tempfile::{tempdir, TempDir};

        const COLLECTION: &str = "mcp-live";
        const TOKEN: &str = "mcp-live-token-1234567890";

        /// Seed a one-doc lex index for `COLLECTION` under the caller's held
        /// `ONEBRAIN_CACHE_DIR`, and write the vault's `onebrain.yml`.
        fn seed_vault(vault: &Path) {
            std::fs::write(
                vault.join("onebrain.yml"),
                format!("search:\n  collection: {COLLECTION}\n"),
            )
            .unwrap();
            let cache_dir = collection_cache_dir(COLLECTION);
            let mut lex = LexIndex::open(&cache_dir.join("tantivy")).unwrap();
            lex.add(&Chunk {
                chunk_id: "alpha.md#0".to_string(),
                doc_path: "alpha.md".to_string(),
                heading_path: String::new(),
                text: "the quick brown fox jumps".to_string(),
                chunk_index: 0,
            })
            .unwrap();
            lex.commit().unwrap();
            // A readable doc on disk so `get`/status fs reads have something.
            std::fs::write(vault.join("alpha.md"), "the quick brown fox jumps").unwrap();
        }

        /// A live daemon-style server holding ONE engine, on an ephemeral port.
        struct LiveServer {
            info: DaemonInfo,
            stop: Arc<AtomicBool>,
            join: Option<std::thread::JoinHandle<()>>,
        }
        impl Drop for LiveServer {
            fn drop(&mut self) {
                self.stop.store(true, Ordering::SeqCst);
                if let Some(j) = self.join.take() {
                    let _ = j.join();
                }
            }
        }

        /// Stand up the server (engine held) and return its handle. The CALLER
        /// must hold the `ONEBRAIN_CACHE_DIR` env guard for the test's lifetime
        /// — the server thread only reads the process env, never locks it.
        fn start_live_server(vault: &Path) -> LiveServer {
            let stop = Arc::new(AtomicBool::new(false));
            let port = Arc::new(AtomicU16::new(0));
            let vault_path = vault.to_path_buf();
            let stop_t = stop.clone();
            let port_t = port.clone();

            let join = std::thread::spawn(move || {
                let mut cfg = crate::server::ServeConfig::localhost(
                    Some(vault_path),
                    0,
                    TOKEN.to_string(),
                    None,
                );
                cfg.hold_engine = true;
                let (router, _state) = crate::server::build_router_with_state(cfg);
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async move {
                    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
                    let on_bind =
                        |a: std::net::SocketAddr| port_t.store(a.port(), Ordering::SeqCst);
                    let shutdown = async move {
                        while !stop_t.load(Ordering::SeqCst) {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    };
                    let _ = crate::server::run_server_from_router(router, addr, on_bind, shutdown)
                        .await;
                });
            });

            let deadline = Instant::now() + Duration::from_secs(5);
            let bound = loop {
                let p = port.load(Ordering::SeqCst);
                if p != 0 {
                    break p;
                }
                assert!(Instant::now() < deadline, "server never bound");
                std::thread::sleep(Duration::from_millis(10));
            };

            LiveServer {
                info: DaemonInfo {
                    port: bound,
                    token: TOKEN.to_string(),
                    pid: std::process::id(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                },
                stop,
                join: Some(join),
            }
        }

        fn resolved_for(vault: &Path) -> ResolvedVault {
            crate::vault_ctx::require(Some(vault.to_path_buf())).unwrap()
        }

        fn lex_query() -> Parameters<QueryParams> {
            Parameters(QueryParams {
                searches: vec![SubQuery {
                    r#type: SubQueryType::Lex,
                    query: "quick".to_string(),
                }],
                limit: Some(5),
                min_score: None,
                candidate_limit: None,
                collections: None,
                intent: None,
                rerank: None,
            })
        }

        fn block_on<F: std::future::Future>(f: F) -> F::Output {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(f)
        }

        // THE CORE WIN: two MCP servers, each daemon-backed, both query the
        // SAME single-engine server concurrently. If either had opened its own
        // engine, redb would refuse with "Database already open" — that this
        // succeeds proves the MCP server holds NO engine and N sessions coexist.
        #[cfg(unix)]
        #[test]
        fn two_daemon_backed_mcp_sessions_query_one_engine() {
            let vault = tempdir().unwrap();
            let cache = tempdir().unwrap();
            let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
            seed_vault(vault.path());
            let srv = start_live_server(vault.path());

            let mk = || {
                McpServer::with_daemon(
                    DaemonHandle::new(srv.info.clone()),
                    resolved_for(vault.path()),
                )
            };
            let (a, b) = (mk(), mk());

            let out_a = block_on(a.query(lex_query())).expect("session A query");
            let out_b = block_on(b.query(lex_query())).expect("session B query");
            assert_eq!(out_a.0.results.len(), 1, "A: {:?}", out_a.0.results);
            assert_eq!(out_b.0.results.len(), 1, "B: {:?}", out_b.0.results);
            assert_eq!(out_a.0.results[0].file, "alpha.md");
            assert_eq!(out_b.0.results[0].file, "alpha.md");
            // Neither returned the no-index note — both actually hit the engine.
            assert!(out_a.0.note.is_none() && out_b.0.note.is_none());

            // status also routes over HTTP on both, no second engine opened.
            let st_a = block_on(a.status()).expect("session A status");
            let st_b = block_on(b.status()).expect("session B status");
            assert_eq!(st_a.0.doc_count(), st_b.0.doc_count());
            assert!(
                st_a.0.doc_count().is_some(),
                "daemon status has a doc_count"
            );
        }

        // Fallback: with NO daemon, a lone MCP server opens the engine directly
        // (sole owner → no lock race) and answers query + status. This is the
        // `Backend::Direct` path `run()` falls back to when `ensure_running`
        // fails — exercised here without spawning a process.
        #[test]
        fn direct_engine_fallback_answers_query_and_status() {
            let vault = tempdir().unwrap();
            let cache = tempdir().unwrap();
            let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
            seed_vault(vault.path());

            // Exactly the fallback path `run()` takes: open_engine, then
            // with_direct_engine (sole owner → no lock race).
            let (engine, resolved) =
                crate::commands::search_common::open_engine(Some(vault.path().to_path_buf()))
                    .expect("direct engine open");
            let server = McpServer::with_direct_engine(engine, resolved);

            let out = block_on(server.query(lex_query())).expect("direct query");
            assert_eq!(out.0.results.len(), 1, "direct: {:?}", out.0.results);
            assert_eq!(out.0.results[0].file, "alpha.md");

            let st = block_on(server.status()).expect("direct status");
            // The lex-only seed doesn't populate the engine's redb, so the
            // engine reports 0 docs — the point is that status READS a real
            // count (Some, not None/busy) off the directly-owned engine.
            assert!(
                st.0.doc_count().is_some(),
                "direct status must read a doc_count off the owned engine, got None (busy?)"
            );
        }

        // Guard: `with_engine` on a daemon-backed server must error, not panic
        // or silently succeed — the daemon path owns no local engine.
        #[test]
        fn with_engine_errors_on_daemon_backend() {
            let vault = tempdir().unwrap();
            let cache = TempDir::new().unwrap();
            let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
            seed_vault(vault.path());
            // A DaemonHandle pointing nowhere is fine — with_engine never calls it.
            let handle = DaemonHandle::new(DaemonInfo {
                port: 1,
                token: TOKEN.to_string(),
                pid: std::process::id(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            });
            let server = McpServer::with_daemon(handle, resolved_for(vault.path()));
            let res = block_on(server.with_engine(|_eng| Ok(())));
            assert!(res.is_err(), "with_engine must error on daemon backend");
        }
    }
}
