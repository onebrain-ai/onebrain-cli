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

use onebrain_core::config::{TokenOptimizationConfig, VaultConfig};
use onebrain_token::gain::Surface;
use onebrain_token::{
    CacheKind, GainEvent, LedgerVerdict, MemoKey, OptLevel, ReferenceEnvelope, TokenCache,
};

use super::daemon_client::{self, DaemonHandle};
use super::search_common::{
    collection_cache_dir, collection_for, index_artifact_path, open_engine,
};
use super::search_status::{
    status_data_for, status_data_from_daemon, DaemonStatusCounts, SearchStatusData,
};
use super::token_runner;

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

/// Prefix on a `get` response that returns an already-sent reference instead
/// of the body (design §3b) — tells the agent why the body isn't inline and
/// how to force it.
const REFERENCE_NOTE: &str = "⤷ already sent this session (unchanged) — reference below; \
     read `rematerialize` to fetch the full body:";

/// Best-effort session-token resolution for the Direct-mode ledger (design
/// §3b) — same resolution the daemon client uses on the daemon path. `None`
/// when no token is resolvable (headless / odd host): the ledger is then
/// silently inactive for the call and the body inlines, never guessed.
fn resolve_session_token_opt() -> Option<String> {
    let inputs = onebrain_cache::ResolveInputs::from_env();
    onebrain_cache::resolve_session_token(&inputs)
        .ok()
        .map(|t| t.to_string())
}

/// Epoch seconds, for surface-recorded gain events (the cache-hit paths that
/// bypass the funnel's own timestamp).
fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

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
    /// Re-materialize the FULL body, bypassing both the already-sent ledger and
    /// the size cap (design §3c). Set this when a reference envelope pointed you
    /// back here — it always returns the complete document.
    pub force: Option<bool>,
    /// Per-call optimization level override ('off'|'conservative'|'balanced'|'aggressive').
    /// Precedence: this > onebrain.yml `token_optimization.level` > default.
    #[serde(rename = "optLevel")]
    pub opt_level: Option<String>,
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
/// for qmd compatibility but are not per-request controls the native engine
/// reads (see field docs) — `rerank` in particular IS implemented (v3.4.7)
/// but as vault-wide config, not a request param.
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
    /// Accepted for qmd compatibility; native rerank is implemented (v3.4.7,
    /// `search.reranker.*` in `onebrain.yml`) but engine-wide/config-driven,
    /// not a per-request toggle — this field stays deserialize-only.
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

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct QueryHit {
    pub docid: String, // chunk id
    pub file: String,  // vault-relative doc path
    pub title: String, // file stem
    pub score: f64,    // normalized 0-1 (top fused hit = 1.0)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>, // heading path, if any
    pub snippet: String,
    /// Calibrated 0-1 Tier-2 cross-encoder relevance, mirroring
    /// `onebrain_search::engine::Hit::rerank_score`. `None` (omitted, matching
    /// `context`'s optional-field style) for lex-only sub-queries and for any
    /// hit the rerank stage skipped (disabled, model not downloaded, or a
    /// rerank failure).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rerank_score: Option<f32>,
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
                .and_modify(|(total, kept)| {
                    *total += s;
                    // A duplicate across sub-queries keeps the first-seen
                    // `Hit` for position/fusion semantics, but that must not
                    // silently drop a later duplicate's real `rerank_score` —
                    // if the kept hit has none yet and this one does, adopt it.
                    if kept.rerank_score.is_none() {
                        kept.rerank_score = hit.rerank_score;
                    }
                })
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
    index_artifact_path(cache_dir, "tantivy").is_dir()
}

/// Final `query`-tool result assembly (design A1's rerank-after-RRF stage,
/// pulled out as a pure function so the skip-not-fail decision is unit
/// testable without a live engine or daemon). `survivors` is the fused,
/// `min_score`-filtered pool in RRF order; `rerank_outcome` is `None` when
/// there were no survivors to rerank, `Some(Ok(hits))` when a rerank call
/// returned (possibly empty) hits, and `Some(Err(_))` when the rerank call
/// itself failed.
///
/// Skip-not-fail: a rerank error, or an `Ok` result that resolved to zero
/// hits (e.g. lex-only chunk ids with no metadata row to resolve against —
/// `resolve_hits` silently drops those), falls back to the fused order with
/// `rerank_score: None` on every hit — the query must never fail, or lose
/// results outright, just because reranking didn't pan out.
fn resolve_query_results(
    survivors: Vec<(f64, Hit)>,
    rerank_outcome: Option<anyhow::Result<Vec<Hit>>>,
    limit: usize,
) -> Vec<QueryHit> {
    let reranked = match rerank_outcome {
        Some(Ok(hits)) if !hits.is_empty() => Some(hits),
        Some(Ok(_)) => {
            eprintln!(
                "onebrain mcp: query-tool rerank resolved 0 of {} candidates — falling back to fused order",
                survivors.len()
            );
            None
        }
        Some(Err(e)) => {
            eprintln!(
                "onebrain mcp: query-tool rerank failed — falling back to fused order: {e:#}"
            );
            None
        }
        None => None,
    };

    match reranked {
        Some(hits) => hits.into_iter().take(limit).map(QueryHit::from).collect(),
        None => survivors
            .into_iter()
            .take(limit)
            .map(|(score, mut hit)| {
                hit.score = score;
                // Skip-not-fail contract: when the post-fusion rerank stage did
                // not run (error / empty / no survivors), the returned hits must
                // carry NO rerank_score. A fused survivor may already hold a
                // rerank_score from a fetch-time rerank pass (vec/hyde sub-query
                // or the daemon's hybrid mode) that was scored against a
                // DIFFERENT sub-query's text — surfacing it here would be a
                // stale, mislabeled confidence number. Clear it.
                hit.rerank_score = None;
                QueryHit::from(hit)
            })
            .collect(),
    }
}

/// Map `QueryHit`s to the canonical hit-list JSON the query transforms
/// understand (design §1: shared transform layer, per-surface field names).
/// The transforms key on `doc_path` (dedup identity) and `text` (snippet
/// cap/disclosure), so `file`→`doc_path` and `snippet`→`text` are renamed in;
/// every other field (docid/title/score/context/rerank_score) rides along
/// untouched. [`decanonicalize_hits`] reverses it.
fn canonicalize_hits(hits: &[QueryHit]) -> Vec<serde_json::Value> {
    hits.iter()
        .map(|h| {
            let mut v = serde_json::to_value(h).unwrap_or(serde_json::Value::Null);
            if let Some(obj) = v.as_object_mut() {
                if let Some(file) = obj.remove("file") {
                    obj.insert("doc_path".to_string(), file);
                }
                if let Some(snippet) = obj.remove("snippet") {
                    obj.insert("text".to_string(), snippet);
                }
            }
            v
        })
        .collect()
}

/// Reverse [`canonicalize_hits`]: `doc_path`→`file`, `text`→`snippet` (absent
/// after `disclosure` stripped it → empty snippet, matching `QueryHit`'s
/// required `String` field), then deserialize back. A hit that fails to
/// round-trip is dropped rather than corrupting the result set.
fn decanonicalize_hits(values: Vec<serde_json::Value>) -> Vec<QueryHit> {
    values
        .into_iter()
        .filter_map(|mut v| {
            if let Some(obj) = v.as_object_mut() {
                if let Some(dp) = obj.remove("doc_path") {
                    obj.insert("file".to_string(), dp);
                }
                let text = obj
                    .remove("text")
                    .unwrap_or_else(|| serde_json::Value::String(String::new()));
                obj.insert("snippet".to_string(), text);
            }
            serde_json::from_value::<QueryHit>(v).ok()
        })
        .collect()
}

/// A stable, normalized descriptor of a `query` request's sub-queries for the
/// memo key — `"<type>:<trimmed query>"` per sub-query, newline-joined, in the
/// caller's order (order matters: the first sub-query gets 2x fusion weight).
fn query_norm(searches: &[SubQuery]) -> String {
    searches
        .iter()
        .map(|s| {
            let ty = match s.r#type {
                SubQueryType::Lex => "lex",
                SubQueryType::Vec => "vec",
                SubQueryType::Hyde => "hyde",
            };
            format!("{ty}:{}", s.query.trim())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The RRF fusion weights `rrf_fuse` applies for `n` sub-queries — the first
/// gets 2x, the rest 1x. Folded into the memo key so a different sub-query
/// arity can't alias.
fn fusion_weights(n: usize) -> Vec<f64> {
    (0..n).map(|i| if i == 0 { 2.0 } else { 1.0 }).collect()
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
    let lex = LexIndex::open(&index_artifact_path(&cache_dir, "tantivy"))
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
                // Lex-only path — the rerank stage only runs on query/vsearch.
                rerank_score: None,
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
/// `{ "hits": [ { "path", "score", "title", "snippet", "rerank_score"? }, .. ], "mode": ".." }`
/// — `rerank_score` is present only when the daemon's hybrid/vector path
/// actually reranked the hit (the producer's `#[serde(skip_serializing_if =
/// "Option::is_none")]`), so a missing key and an unreranked hit are the same
/// thing: `None`.
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
        // A non-numeric `rerank_score` (contract break) degrades to `None`
        // rather than erroring the whole search — an unreranked hit is a
        // normal, expected shape, so this stays lenient like the other
        // optional fields above (`title`/`snippet` default to "" on a bad
        // type instead of failing the request).
        let rerank_score = h
            .get("rerank_score")
            .and_then(|v| v.as_f64())
            .map(|s| s as f32);
        out.push(Hit {
            chunk_id: path.clone(),
            doc_path: path,
            heading_path,
            score,
            snippet,
            rerank_score,
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
            rerank_score: h.rerank_score,
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

    /// Load this vault's full config (default on any read error — a config
    /// problem must never fail a read tool).
    fn vault_config(&self) -> VaultConfig {
        onebrain_core::config::load_vault_config_at(self.resolved.root.as_path())
            .unwrap_or_default()
    }

    /// Load this vault's `token_optimization` config (default on any read
    /// error — a config problem must never fail a read tool).
    fn opt_config(&self) -> TokenOptimizationConfig {
        self.vault_config().token_optimization
    }

    /// `<collection_cache>/token/token.redb` for Direct-mode memoization — the
    /// daemon owns this file in daemon mode, so the memo path is Direct-only
    /// (this release adds no daemon memo route). Creates the parent dir.
    fn token_db_path(&self) -> Option<PathBuf> {
        let collection = collection_for(&self.resolved).ok()?;
        let dir = collection_cache_dir(&collection).join("token");
        std::fs::create_dir_all(&dir).ok()?;
        Some(dir.join("token.redb"))
    }

    /// Read a memoized result set for `key` (Direct mode). Best-effort — any
    /// open/read failure is a clean miss, never an error.
    fn memo_get(&self, key: &MemoKey) -> Option<String> {
        let path = self.token_db_path()?;
        let cache = TokenCache::open(&path).ok()?;
        cache.memo().get(key).ok().flatten()
    }

    /// Store `results` under `key` (Direct mode). Best-effort — a write failure
    /// is swallowed so a cache hiccup never fails a live query.
    fn memo_put(&self, key: &MemoKey, results: &str) {
        if let Some(path) = self.token_db_path() {
            if let Ok(cache) = TokenCache::open(&path) {
                let _ = cache.memo().put(key, results);
            }
        }
    }

    /// Resolve the effective level for a tool call (per-call > config >
    /// default), falling back to the config level on an unparseable per-call
    /// override rather than failing the call.
    fn resolve_level(&self, per_call: Option<&str>, cfg: &TokenOptimizationConfig) -> OptLevel {
        token_runner::resolve_level(per_call, cfg).unwrap_or(cfg.level)
    }

    /// Check the already-sent ledger for `doc_path` and, on an `unchanged`
    /// verdict, return the reference envelope the caller sends instead of the
    /// body (design §3b). `record: true` semantics — the FIRST send is stamped
    /// so the next repeat is caught. Works in BOTH backends (design §5d):
    ///
    /// - **Daemon**: the daemon owns `token.redb`; go through
    ///   `POST /api/token/ledger/check`, which also enforces the M3 level gate.
    /// - **Direct**: no daemon, so open `token.redb` in-process and run the
    ///   ledger against the locally-owned engine's `doc_hash`. The caller has
    ///   already applied the level gate (`level >= Balanced`) before calling.
    ///
    /// Any other verdict (`first_send`/`changed`/`inactive`/`no_session`/
    /// `unknown_doc`), a 404 old daemon, an unresolvable session token, or any
    /// error yields `None` → the caller inlines the full body. `full_len` is the
    /// body size credited to `bytes_saved` on the Direct path.
    async fn ledger_reference(&self, doc_path: String, full_len: u64) -> Option<ReferenceEnvelope> {
        match &self.backend {
            Backend::Daemon(handle) => {
                let handle = handle.clone();
                let path = doc_path.clone();
                let value =
                    tokio::task::spawn_blocking(move || handle.token_ledger_check(&path, true))
                        .await
                        .ok()?
                        .ok()??; // JoinError / anyhow::Err / Ok(None) all → None
                if value.get("verdict").and_then(|v| v.as_str()) != Some("unchanged") {
                    return None;
                }
                serde_json::from_value::<ReferenceEnvelope>(value.get("reference")?.clone()).ok()
            }
            Backend::Direct(_) => {
                // No daemon → resolve the session locally and run the ledger
                // over the in-process engine + `token.redb`. No session token
                // resolvable → ledger inactive for this call (never guessed).
                let session = resolve_session_token_opt()?;
                let db_path = self.token_db_path()?;
                let path = doc_path.clone();
                self.with_engine(move |eng| {
                    let Some(hash) = eng.doc_hash(&path) else {
                        return Ok(None); // unindexed doc → can't claim unchanged
                    };
                    let cache = TokenCache::open(&db_path).map_err(|e| anyhow::anyhow!("{e}"))?;
                    let ledger = cache.ledger();
                    match ledger
                        .check(&session, &path, &hash)
                        .map_err(|e| anyhow::anyhow!("{e}"))?
                    {
                        LedgerVerdict::Unchanged { sent_hash } => Ok(Some(ReferenceEnvelope::new(
                            path.clone(),
                            sent_hash,
                            full_len,
                        ))),
                        LedgerVerdict::FirstSend | LedgerVerdict::Changed => {
                            ledger
                                .record(&session, &path, &hash)
                                .map_err(|e| anyhow::anyhow!("{e}"))?;
                            Ok(None)
                        }
                    }
                })
                .await
                .ok()
                .flatten()
            }
        }
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
        let has_lex = params
            .searches
            .iter()
            .any(|s| matches!(s.r#type, SubQueryType::Lex));
        // Capture the primary (2x-weighted) sub-query text BEFORE `params.searches`
        // is consumed below — this is the query the post-fusion rerank stage scores
        // candidates against.
        let primary_query = params.searches[0].query.clone();

        let collection = collection_for(&self.resolved).map_err(internal)?;
        let cache_dir = collection_cache_dir(&collection);
        if !tantivy_index_present(&cache_dir) {
            return Ok(Json(QueryOut {
                results: vec![],
                note: Some(no_index_note()),
            }));
        }

        let vault_cfg = self.vault_config();
        let opt_cfg = &vault_cfg.token_optimization;
        let level = self.resolve_level(None, opt_cfg);
        let q_norm = query_norm(&params.searches);
        let weights = fusion_weights(params.searches.len());

        // Query-result memoization (design §3a / M2) — Direct mode only (the
        // daemon owns `token.redb`; no daemon memo route this release). The key
        // folds the index identity (generation + nonce) AND every
        // result-determining knob that changes without a reindex (reranker
        // on/off + model, the rerank-score gate, fusion weights), so a config
        // change can never serve stale hits.
        let memo_key = if matches!(self.backend, Backend::Direct(_)) {
            let (generation, index_nonce) = self
                .with_engine(|e| Ok((e.generation(), e.index_nonce())))
                .await?;
            let r = &vault_cfg.search.reranker;
            Some(MemoKey::new(
                &q_norm,
                "mcp",
                limit,
                r.min_candidates,
                min_score as f32,
                generation,
                &index_nonce,
                r.enabled,
                &r.model,
                r.min_score.unwrap_or(0.30),
                &weights,
            ))
        } else {
            None
        };

        // Memo HIT: skip embedding + rerank entirely, shape the cached result
        // set for the current level, and record a single `MemoHit` gain event.
        if let Some(key) = &memo_key {
            if let Some(cached) = self.memo_get(key) {
                if let Ok(cached_hits) = serde_json::from_str::<Vec<QueryHit>>(&cached) {
                    let before = cached.len() as u64;
                    let ctx = token_runner::ctx_for(level, opt_cfg, None, false, None);
                    let shaped = decanonicalize_hits(token_runner::shape_query_hits_unmetered(
                        Surface::McpQuery,
                        canonicalize_hits(&cached_hits),
                        &ctx,
                    ));
                    let after = serde_json::to_string(&shaped)
                        .map(|s| s.len() as u64)
                        .unwrap_or(before);
                    token_runner::record_gain(
                        &self.resolved,
                        GainEvent {
                            ts: now_ts(),
                            surface: Surface::McpQuery,
                            transform: "memo".to_string(),
                            level,
                            bytes_before: before,
                            bytes_after: after,
                            cache: CacheKind::MemoHit,
                            session_token: None,
                        },
                    );
                    return Ok(Json(QueryOut {
                        results: shaped,
                        note: None,
                    }));
                }
            }
        }

        let ranked: Vec<(f64, Vec<Hit>)> = match &self.backend {
            // Daemon path: route EACH sub-query through the daemon's held engine
            // over HTTP (`GET /api/vault/search`) — lex → `lex`, vec/hyde →
            // `hybrid`. The MCP server opens no engine, so N concurrent MCP
            // sessions all fan out to the one daemon with no redb contention.
            //
            // Behavioral deltas vs the Direct path (documented in docs/daemon.md
            // + ADR 0023): the daemon returns DOC-LEVEL hits (dedup/fusion keys
            // on `path`, so at most one hit per document), and a `vec` sub-query
            // is served as `hybrid` (lex mixed in) on the wire. Fusion depth
            // matches the Direct arm: we pass the SAME `fetch_k` over-fetch as
            // an explicit `top_k` so RRF has depth to fuse. Before v3.4.7 the
            // server hardcoded 20 here; passing `None` would now fall to the
            // vault's `search.default_top_k` (10) and shrink fusion depth.
            Backend::Daemon(handle) => {
                let handle = handle.clone();
                let fetch_k = limit.max(10) * 3;
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
                                .search(&sub.query, mode, Some(fetch_k), None)
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
            // Over-fetches `fetch_k` (≥30) chunk-level candidates per sub-query
            // so RRF has depth to fuse; this path can return multiple chunks of
            // one doc, unlike the doc-level daemon path.
            Backend::Direct(_) => {
                let fetch_k = limit.max(10) * 3;
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
        let survivors: Vec<(f64, Hit)> = fused
            .into_iter()
            .filter(|(score, _)| *score >= min_score)
            .collect();

        // Rerank stage (design A1): rerank the fused, min_score-filtered pool
        // against the primary sub-query text, branching on the same backend the
        // fetch used above. Skip-not-fail: any rerank error, OR a resolved pool
        // that came back empty (e.g. lex-only chunk ids with no metadata row to
        // resolve against — `resolve_hits` silently drops those, same as the
        // engine's own query/vector_search paths), keeps the current fused
        // order with `rerank_score: None` — the query must never fail, or lose
        // results outright, because reranking didn't pan out.
        let rerank_outcome: Option<anyhow::Result<Vec<Hit>>> = if survivors.is_empty() {
            None
        } else {
            Some(match &self.backend {
                Backend::Direct(_) => {
                    let fused_pairs: Vec<(String, f64)> = survivors
                        .iter()
                        .map(|(score, hit)| (hit.chunk_id.clone(), *score))
                        .collect();
                    let query = primary_query.clone();
                    self.with_engine(move |eng| eng.rerank_hits(&query, fused_pairs, limit))
                        .await
                        .map_err(|e| anyhow::anyhow!(e))
                }
                Backend::Daemon(handle) => {
                    let handle = handle.clone();
                    // Send the full fused survivor set (RRF-ordered, best
                    // first). `Engine::rerank_paths` bounds the actual
                    // cross-encode cost itself — it round-robins at most
                    // `max(min_candidates, top_k)` chunks across these paths, so
                    // a long-doc vault can't blow up the daemon rerank and the
                    // vault's configured `min_candidates` is honored (rather
                    // than a hardcoded cap here that couldn't see it).
                    let paths: Vec<String> = survivors
                        .iter()
                        .map(|(_, hit)| hit.doc_path.clone())
                        .collect();
                    // The daemon's doc-level `/api/internal/rerank` seeds fused
                    // scores at 0.0 (it ranks by the cross-encoder, not the
                    // fused rank), so its hits come back with `score: 0.0`.
                    // Re-stamp the real fused RRF score from `survivors` by
                    // doc_path — daemon survivors are doc-level (one hit per
                    // path), so this is a 1:1 map. The Direct arm already
                    // threads the true fused score through `rerank_hits`, so
                    // only the daemon path needs this restamp.
                    let fused_by_path: std::collections::HashMap<String, f64> = survivors
                        .iter()
                        .map(|(score, hit)| (hit.doc_path.clone(), *score))
                        .collect();
                    let query = primary_query.clone();
                    tokio::task::spawn_blocking(move || {
                        let value = handle.rerank(&query, &paths, limit)?;
                        let mut hits = parse_daemon_search_hits(&value)?;
                        for hit in &mut hits {
                            if let Some(score) = fused_by_path.get(&hit.doc_path) {
                                hit.score = *score;
                            }
                        }
                        Ok(hits)
                    })
                    .await
                    .map_err(|e| anyhow::anyhow!(e))
                    .and_then(|inner| inner)
                }
            })
        };

        let results = resolve_query_results(survivors, rerank_outcome, limit);

        // Memo MISS: cache the resolved (pre-shaping) result set so a repeat of
        // the same query skips embedding + rerank. Memoization is
        // level-independent — token shaping is presentation, applied fresh on
        // both hit and miss, so it stays out of the memo value and the key.
        if let Some(key) = &memo_key {
            if let Ok(serialized) = serde_json::to_string(&results) {
                self.memo_put(key, &serialized);
            }
        }

        // Shape the fused/reranked hit list through the funnel (McpQuery:
        // json_compact + doc_dedup at conservative, snippet at balanced,
        // disclosure at aggressive) — records one gain event.
        let ctx = token_runner::ctx_for(level, opt_cfg, None, false, None);
        let shaped = decanonicalize_hits(token_runner::shape_query_hits(
            &self.resolved,
            Surface::McpQuery,
            canonicalize_hits(&results),
            &ctx,
        ));

        Ok(Json(QueryOut {
            results: shaped,
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

        let resolved_path = {
            let path_for_resolve = path_part.clone();
            tokio::task::spawn_blocking(move || resolve_under_vault(&vault_root, &path_for_resolve))
                .await
                .map_err(internal)?
                .map_err(|e| ErrorData::invalid_params(e.to_string(), None))?
        };

        let text = tokio::fs::read_to_string(&resolved_path)
            .await
            .map_err(|e| {
                ErrorData::invalid_params(format!("reading {}: {e}", params.file), None)
            })?;

        let from_line = suffix_line.or(params.from_line);
        let windowed = from_line.is_some() || params.max_lines.is_some();

        let cfg = self.opt_config();
        let level = self.resolve_level(params.opt_level.as_deref(), &cfg);
        let force = params.force.unwrap_or(false);

        // Already-sent ledger (design §3b, level 2↑): a repeat, unchanged,
        // WHOLE-doc read comes back as a reference the agent can re-materialize
        // with `--force`, not the full body again. Works in both backends
        // (daemon route or in-process `token.redb`, design §5d). The level gate
        // is applied here; `force` and windowed reads always inline.
        if !force && !windowed && level >= OptLevel::Balanced {
            let full_bytes = text.len() as u64;
            if let Some(reference) = self.ledger_reference(path_part.clone(), full_bytes).await {
                let ref_json =
                    serde_json::to_string_pretty(&reference).unwrap_or_else(|_| String::new());
                token_runner::record_gain(
                    &self.resolved,
                    GainEvent {
                        ts: now_ts(),
                        surface: Surface::McpGet,
                        transform: "ledger_ref".to_string(),
                        level,
                        bytes_before: full_bytes,
                        bytes_after: ref_json.len() as u64,
                        cache: CacheKind::LedgerRef,
                        session_token: None,
                    },
                );
                let body = format!("{REFERENCE_NOTE}\n{ref_json}");
                return Ok(CallToolResult::success(vec![ContentBlock::text(body)]));
            }
        }

        let sliced = slice_lines(
            &text,
            from_line,
            params.max_lines,
            params.line_numbers.unwrap_or(false),
        );

        // Shape the doc body through the funnel (whitespace / frontmatter /
        // get_cap per level + surface). `force` bypasses the size cap.
        let ctx = token_runner::ctx_for(level, &cfg, Some(path_part.clone()), force, None);
        let shaped = token_runner::shape_document(&self.resolved, Surface::McpGet, sliced, &ctx);
        Ok(CallToolResult::success(vec![ContentBlock::text(shaped)]))
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

        // Shape the combined fan-out body once (whitespace + head_only per
        // level) — a single funnel call, so one gain event lands per multi_get
        // call (the §5d "exactly one GainEvent per optimized call" rule). Uses
        // the config-resolved level; multi_get has no per-call override.
        let cfg = self.opt_config();
        let level = self.resolve_level(None, &cfg);
        let ctx = token_runner::ctx_for(level, &cfg, None, false, None);
        let shaped = token_runner::shape_document(
            &self.resolved,
            Surface::McpMultiGet,
            sections.join("\n\n"),
            &ctx,
        );
        Ok(CallToolResult::success(vec![ContentBlock::text(shaped)]))
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

    // A daemon `ensure_running` spawns resolves ITS vault from the `--vault` arg
    // we pass it (its detached child chdir's to `/`, so walk-up is useless — see
    // `daemon::resolve_daemon_vault`). We thread `resolved.root` through
    // `ensure_running` → `spawn_daemon_start` → `daemon start --vault <path>`,
    // so the daemon we auto-start binds the SAME vault we resolved WITHOUT
    // mutating this process's `$ONEBRAIN_VAULT` (a `std::env::set_var`, which is
    // unsound under concurrent reads and deprecated since Rust 1.81). `resolved`
    // already honoured the canonical precedence (flag > env > cwd), so it is
    // authoritative for THIS process and the daemon can't disagree.

    // Primary path: route through the warm daemon so the MCP server holds NO
    // redb lock and multiple concurrent MCP sessions coexist. `ensure_running`
    // auto-starts the daemon (the sole engine owner) if it isn't up yet, and —
    // passed our resolved vault — RESTARTS a daemon bound to a DIFFERENT vault so
    // `query`/`status` never silently route through the wrong vault's engine
    // (one vault at a time per machine; switching vaults restarts the daemon).
    //
    // Fallback: if the daemon genuinely can't start (e.g. spawn failed), open
    // the engine directly so a lone session still works — today's behaviour.
    // This is the ONLY safe direct-open: as the sole owner there's no lock race.
    // `ONEBRAIN_NO_DAEMON` kill-switch: skip discovery/auto-start entirely and
    // open the engine directly — the same escape hatch the search verbs honor
    // (`search_common::daemon_routing_disabled`). Direct mode is fully
    // supported (memoization + ledger open `token.redb` in-process, design
    // §5d), so this is a first-class path, not a degraded one.
    let server = if super::search_common::daemon_routing_disabled() {
        let (engine, resolved) = open_engine(vault_flag)?;
        McpServer::with_direct_engine(engine, resolved)
    } else {
        match daemon_client::ensure_running(Some(resolved.root.as_path())) {
            Ok(handle) => McpServer::with_daemon(handle, resolved),
            Err(daemon_err) => {
                tracing::warn!(
                    error = %daemon_err,
                    "warm daemon unavailable; falling back to a direct single-process engine open"
                );
                let (engine, resolved) = open_engine(vault_flag).with_context(|| {
                    format!(
                        "daemon unavailable ({daemon_err:#}) and direct engine open also failed"
                    )
                })?;
                McpServer::with_direct_engine(engine, resolved)
            }
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
            rerank_score: None,
        }
    }

    // ── Token-surface shaping helpers (Track 4) ────────────────────────────

    fn query_hit(file: &str, snippet: &str, rerank: Option<f32>) -> QueryHit {
        QueryHit {
            docid: format!("{file}#0"),
            file: file.into(),
            title: file.trim_end_matches(".md").into(),
            score: 0.9,
            context: Some("Heading".into()),
            snippet: snippet.into(),
            rerank_score: rerank,
        }
    }

    #[test]
    fn canonicalize_renames_file_and_snippet_for_the_transforms() {
        let hits = vec![query_hit("a.md", "hello world", Some(0.8))];
        let canonical = canonicalize_hits(&hits);
        let obj = canonical[0].as_object().unwrap();
        // Transforms key on `doc_path` (dedup) + `text` (snippet cap/disclosure).
        assert_eq!(obj["doc_path"], "a.md");
        assert_eq!(obj["text"], "hello world");
        assert!(obj.get("file").is_none(), "file renamed away");
        assert!(obj.get("snippet").is_none(), "snippet renamed away");
        // Carry-through fields survive untouched.
        assert_eq!(obj["docid"], "a.md#0");
        assert_eq!(obj["rerank_score"].as_f64().unwrap() as f32, 0.8_f32);
    }

    #[test]
    fn canonical_round_trip_preserves_hits() {
        let hits = vec![
            query_hit("a.md", "one", Some(0.7)),
            query_hit("b.md", "two", None),
        ];
        let back = decanonicalize_hits(canonicalize_hits(&hits));
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].file, "a.md");
        assert_eq!(back[0].snippet, "one");
        assert_eq!(back[0].rerank_score, Some(0.7));
        assert_eq!(back[1].file, "b.md");
        assert_eq!(back[1].rerank_score, None);
    }

    #[test]
    fn decanonicalize_missing_text_yields_empty_snippet() {
        // disclosure removes `text` at aggressive — a hit with no text must
        // deserialize to an empty snippet (required String), not be dropped.
        let mut v = serde_json::to_value(query_hit("a.md", "x", None)).unwrap();
        let obj = v.as_object_mut().unwrap();
        let file = obj.remove("file").unwrap();
        obj.insert("doc_path".into(), file);
        obj.remove("snippet");
        // no `text` key at all (disclosure stripped it)
        let back = decanonicalize_hits(vec![v]);
        assert_eq!(back.len(), 1, "hit kept despite missing snippet");
        assert_eq!(back[0].snippet, "");
    }

    #[test]
    fn query_norm_encodes_type_query_and_order() {
        let searches = vec![
            SubQuery {
                r#type: SubQueryType::Lex,
                query: "  errors  ".into(),
            },
            SubQuery {
                r#type: SubQueryType::Vec,
                query: "why fail".into(),
            },
        ];
        assert_eq!(query_norm(&searches), "lex:errors\nvec:why fail");
    }

    #[test]
    fn fusion_weights_first_is_double() {
        assert_eq!(fusion_weights(1), vec![2.0]);
        assert_eq!(fusion_weights(3), vec![2.0, 1.0, 1.0]);
    }

    // ── QueryHit::rerank_score serialization (Task 7) ──────────────────────

    #[test]
    fn query_hit_serializes_rerank_score_when_present() {
        let mut h = hit("a");
        h.rerank_score = Some(0.75);
        let qh = QueryHit::from(h);
        let v = serde_json::to_value(&qh).unwrap();
        let got = v["rerank_score"]
            .as_f64()
            .expect("rerank_score is a number");
        assert!((got - 0.75).abs() < 1e-6, "got {got}");
    }

    #[test]
    fn query_hit_omits_rerank_score_when_none() {
        // Matches `context`'s existing optional-field style: omit the key
        // entirely (not `null`) when there's no rerank score to report.
        let qh = QueryHit::from(hit("a"));
        let v = serde_json::to_value(&qh).unwrap();
        assert!(
            v.get("rerank_score").is_none(),
            "rerank_score must be omitted, not null: {v}"
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // `resolve_query_results` — the `query` tool's post-RRF rerank stage
    // (design A1). Pure function, so both the reranked-order path and every
    // skip-not-fail branch are exercised without a live engine or daemon.
    // ─────────────────────────────────────────────────────────────────

    #[test]
    fn resolve_query_results_uses_reranked_order_and_surfaces_rerank_score() {
        // Fused (RRF) order is a, b, c — the reranker flips it to c, a, b and
        // stamps each hit with a calibrated score. The final results must
        // follow the RERANKED order and carry `rerank_score`, not the fused
        // order/plain fused `score`.
        let survivors = vec![(1.0, hit("a")), (0.6, hit("b")), (0.3, hit("c"))];
        let mut c = hit("c");
        c.rerank_score = Some(0.95);
        let mut a = hit("a");
        a.rerank_score = Some(0.80);
        let mut b = hit("b");
        b.rerank_score = Some(0.40);
        let reranked = vec![c, a, b];

        let results = resolve_query_results(survivors, Some(Ok(reranked)), 10);

        assert_eq!(
            results.iter().map(|r| r.docid.as_str()).collect::<Vec<_>>(),
            vec!["c", "a", "b"],
            "results must follow the reranked order, not fused RRF order"
        );
        assert_eq!(results[0].rerank_score, Some(0.95));
        assert_eq!(results[1].rerank_score, Some(0.80));
        assert_eq!(results[2].rerank_score, Some(0.40));
    }

    #[test]
    fn resolve_query_results_respects_limit_after_rerank() {
        let survivors = vec![(1.0, hit("a")), (0.5, hit("b"))];
        let reranked = vec![hit("b"), hit("a")];
        let results = resolve_query_results(survivors, Some(Ok(reranked)), 1);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].docid, "b");
    }

    #[test]
    fn resolve_query_results_falls_back_to_fused_order_when_rerank_errors() {
        // Skip-not-fail: a rerank `Err` must NOT propagate as a query failure —
        // it keeps the fused RRF order with `rerank_score` absent on every hit.
        let survivors = vec![(1.0, hit("a")), (0.5, hit("b"))];
        let outcome: anyhow::Result<Vec<Hit>> = Err(anyhow::anyhow!("reranker model crashed"));

        let results = resolve_query_results(survivors, Some(outcome), 10);

        assert_eq!(
            results.iter().map(|r| r.docid.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"],
            "must keep fused RRF order on rerank error"
        );
        assert!(results.iter().all(|r| r.rerank_score.is_none()));
        // The fused score itself must still be applied to the fallback hits.
        assert_eq!(results[0].score, 1.0);
        assert_eq!(results[1].score, 0.5);
    }

    #[test]
    fn resolve_query_results_clears_stale_rerank_score_on_fallback() {
        // Regression: a fused survivor may already carry a `rerank_score` from
        // a fetch-time rerank pass (a vec/hyde sub-query, or the daemon's
        // hybrid mode) scored against a DIFFERENT sub-query's text. When the
        // post-fusion rerank stage does NOT run (error / empty resolve), that
        // stale score must be cleared — the fallback contract is "no
        // rerank_score", and surfacing a score computed against another query
        // would mislead. The plain `hit()` fixture starts at `None`, so this
        // seeds `Some(_)` explicitly to lock in the clearing behavior.
        let mut stale = hit("a");
        stale.rerank_score = Some(0.91); // as if from the vec sub-query's fetch-time rerank
        let survivors = vec![(1.0, stale), (0.5, hit("b"))];
        let outcome: anyhow::Result<Vec<Hit>> = Err(anyhow::anyhow!("primary-query rerank failed"));

        let results = resolve_query_results(survivors, Some(outcome), 10);

        assert!(
            results.iter().all(|r| r.rerank_score.is_none()),
            "fallback must clear a fetch-time rerank_score, not leak it"
        );
        assert_eq!(results[0].score, 1.0);
    }

    #[test]
    fn resolve_query_results_falls_back_when_rerank_resolves_to_empty() {
        // A rerank call can return `Ok(vec![])` without erroring (e.g. every
        // candidate chunk id had no metadata row to resolve against). That
        // must be treated the same as an error: fall back to fused order
        // rather than silently returning zero results.
        let survivors = vec![(1.0, hit("a")), (0.5, hit("b"))];
        let results = resolve_query_results(survivors, Some(Ok(Vec::new())), 10);
        assert_eq!(
            results.iter().map(|r| r.docid.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert!(results.iter().all(|r| r.rerank_score.is_none()));
    }

    #[test]
    fn resolve_query_results_no_survivors_is_empty_regardless_of_outcome() {
        // No fused survivors → nothing to rerank; `rerank_outcome` is `None`
        // (the caller never issues a rerank call), and the result is empty.
        let results = resolve_query_results(Vec::new(), None, 10);
        assert!(results.is_empty());
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
    fn rrf_fuse_dedup_adopts_later_duplicates_rerank_score() {
        // lex-first duplicate has no rerank_score; vec-second duplicate does.
        // The dedup must NOT silently drop the real score just because the
        // first-seen hit (kept for position/fusion semantics) had `None`.
        let mut lex_dup = hit("x");
        lex_dup.rerank_score = None;
        let mut vec_dup = hit("x");
        vec_dup.rerank_score = Some(0.8);
        let fused = rrf_fuse(vec![(2.0, vec![lex_dup]), (1.0, vec![vec_dup])]);
        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].1.rerank_score, Some(0.8));
    }

    #[test]
    fn rrf_fuse_dedup_keeps_first_seen_rerank_score_when_present() {
        // Reverse order: first-seen duplicate already has a score — it must
        // be kept (not clobbered by a later `None`).
        let mut vec_dup = hit("x");
        vec_dup.rerank_score = Some(0.8);
        let mut lex_dup = hit("x");
        lex_dup.rerank_score = None;
        let fused = rrf_fuse(vec![(2.0, vec![vec_dup]), (1.0, vec![lex_dup])]);
        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].1.rerank_score, Some(0.8));
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
        // A cache dir with no tantivy index → no index.
        assert!(!tantivy_index_present(dir.path()));
        std::fs::create_dir_all(index_artifact_path(dir.path(), "tantivy")).unwrap();
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
        // Neither hit's body carries `rerank_score` (an unreranked daemon
        // response, e.g. lex mode) → both map to None.
        assert!(hits[0].rerank_score.is_none());
        assert!(hits[1].rerank_score.is_none());
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
        assert!(qh0.rerank_score.is_none());
    }

    #[test]
    fn parse_daemon_search_hits_reads_rerank_score_when_present() {
        // The producer (`server::search::SearchHit`) only emits `rerank_score`
        // when the daemon's hybrid path actually reranked the hit
        // (`skip_serializing_if`) — this is the present-key branch of that
        // wire contract.
        let body = serde_json::json!({
            "hits": [
                {"path": "notes/a.md", "score": 1.0, "title": "a", "snippet": "", "rerank_score": 0.93}
            ],
            "mode": "hybrid"
        });
        let hits = parse_daemon_search_hits(&body).unwrap();
        assert_eq!(hits.len(), 1);
        let score = hits[0].rerank_score.expect("rerank_score must be Some");
        assert!((score - 0.93).abs() < 1e-6, "got {score}");

        // Propagates through into the MCP-facing QueryHit unchanged.
        let qh = QueryHit::from(hits.into_iter().next().unwrap());
        let qh_score = qh.rerank_score.expect("QueryHit must carry rerank_score");
        assert!((qh_score - 0.93).abs() < 1e-6, "got {qh_score}");
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
        use crate::commands::daemon_client::{canonical_vault_id, DaemonHandle, DaemonInfo};
        use crate::commands::search_common::{collection_cache_dir, index_artifact_path};
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
            let mut lex = LexIndex::open(&index_artifact_path(&cache_dir, "tantivy")).unwrap();
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
                    // Stamp the bound vault so a daemon-backed McpServer built
                    // from this info reconnects to (and status/search-matches)
                    // the SAME vault.
                    vault: canonical_vault_id(vault),
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
            // End-to-end skip-not-fail: this vault has no reranker configured
            // and its lex-only seed has no redb chunk metadata, so the
            // rerank stage resolves to nothing and the query tool must fall
            // back to fused RRF order with `rerank_score` absent rather than
            // dropping the (only) result.
            assert!(
                out.0.results[0].rerank_score.is_none(),
                "no reranker configured — rerank_score must be absent"
            );

            let st = block_on(server.status()).expect("direct status");
            // The lex-only seed doesn't populate the engine's redb, so the
            // engine reports 0 docs — the point is that status READS a real
            // count (Some, not None/busy) off the directly-owned engine.
            assert!(
                st.0.doc_count().is_some(),
                "direct status must read a doc_count off the owned engine, got None (busy?)"
            );
        }

        // crit-6: a NON-2xx from the daemon on a lex sub-query must surface as
        // an MCP ERROR from the `query` tool — NEVER a silent empty result set.
        // We point the handle at the live server but with a WRONG token, so
        // `/api/vault/search` returns 401 (a real answer from a live daemon,
        // classified non-retryable → propagates through `with_retry`). The lex
        // sub-query error is NOT degraded (no other lex to fall back to), so the
        // tool errors. This pins the silent-failure guarantee end-to-end.
        #[test]
        fn query_errors_not_empty_on_daemon_non_2xx() {
            let vault = tempdir().unwrap();
            let cache = tempdir().unwrap();
            let _env = crate::test_env::set_var("ONEBRAIN_CACHE_DIR", cache.path());
            seed_vault(vault.path());
            let srv = start_live_server(vault.path());

            // Same port/vault, but a token the server will reject → 401 on search.
            let mut bad = srv.info.clone();
            bad.token = "wrong-token-000000000000".to_string();
            let server = McpServer::with_daemon(DaemonHandle::new(bad), resolved_for(vault.path()));

            let res = block_on(server.query(lex_query()));
            assert!(
                res.is_err(),
                "a daemon non-2xx on a lex sub-query must ERROR, not return an empty result set"
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
                vault: canonical_vault_id(vault.path()),
            });
            let server = McpServer::with_daemon(handle, resolved_for(vault.path()));
            let res = block_on(server.with_engine(|_eng| Ok(())));
            assert!(res.is_err(), "with_engine must error on daemon backend");
        }
    }
}
