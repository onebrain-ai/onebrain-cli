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
use serde::{Deserialize, Serialize};

use crate::cli::SearchQueryArgs;
#[cfg(feature = "semantic")]
use crate::commands::daemon_client::DaemonHandle;
use crate::commands::search_common::{
    collection_cache_dir, index_artifact_path, open_engine, resolve_collection,
};
#[cfg(feature = "semantic")]
use crate::commands::search_common::{
    map_daemon_error, rerank_settings_from_config, route_to_daemon,
};
use crate::commands::token_runner;
use crate::output::{emit, Envelope, OutputMode};
use onebrain_core::ResolvedVault;
use onebrain_search::engine::Hit;
use onebrain_search::lex::LexIndex;
use onebrain_token::gain::Surface;

#[derive(Debug, Serialize, Deserialize)]
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
    /// Calibrated 0–1 cross-encoder relevance from the Tier-2 rerank stage
    /// (see `onebrain_search::engine::Hit::rerank_score`). `None` on the
    /// pure-lex `search` verb (never reranked), on a daemon-routed hit when
    /// the daemon didn't rerank it (old daemon binary predating the wire
    /// field, or the reranker was skipped/unavailable for this query).
    #[serde(skip_serializing_if = "Option::is_none")]
    rerank_score: Option<f32>,
}

impl From<Hit> for HitData {
    fn from(h: Hit) -> Self {
        Self {
            chunk_id: h.chunk_id,
            doc_path: h.doc_path,
            heading_path: h.heading_path,
            score: h.score,
            snippet: h.snippet,
            rerank_score: h.rerank_score,
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
    // Warm-daemon path: when a daemon already holds the engine (e.g. an active
    // `onebrain mcp` session), route the hybrid query through it so the CLI
    // works WITHOUT hitting redb's single-writer lock. Only when the daemon
    // serves this exact vault; otherwise fall through to the direct open below.
    let resolved = crate::vault_ctx::require(vault_flag.clone())?;
    if let Some(handle) = route_to_daemon(&resolved) {
        return run_query_via_daemon(&handle, &resolved, "hybrid", args, mode);
    }

    let (mut engine, resolved) = open_engine(vault_flag)?;
    // `--min-score` is pushed into the rerank gate when reranking is active;
    // `raw_min` is `Some` only when it should instead filter the raw score.
    let raw_min = apply_rerank_overrides(&mut engine, args);
    let vault_info = crate::vault_ctx::info_from(&resolved);
    let hits = apply_min_score(engine.query(&args.text, args.top_k)?, raw_min);
    let hint = if hits.is_empty() {
        index_hint_for(&engine, &resolved)
    } else {
        // `query`'s fused RRF score has no cosine interpretation, so the
        // unreranked fallback gets the generic reranker-not-downloaded line
        // only (no `vec_top_score`).
        rerank_unreranked_hint(&hits, None, engine.rerank_enabled())
    };
    emit_hits(
        "search.query",
        &resolved,
        vault_info,
        hits,
        hint,
        args.opt_level.as_deref(),
        mode,
    )
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

/// Top score ≥ this → a confident vector match (above the e5 no-match ceiling
/// ~0.85, measured 2026-07-06). Below → advisory hint, never a silent drop.
const VEC_CONFIDENT: f64 = 0.86;
/// Top score in `[VEC_AMBIGUOUS, VEC_CONFIDENT)` → low-confidence band; below
/// `VEC_AMBIGUOUS` → likely noise. (Bi-encoder cosine can't cleanly gate e5
/// noise — see the search-quality research; superseded by the reranker.)
const VEC_AMBIGUOUS: f64 = 0.80;

/// Advisory, never-silent confidence label for a vsearch result set, from its
/// best cosine score. Returns `None` when confident (no hint needed); otherwise
/// a hint that the results may be weak — so a low-confidence result reads as
/// honest rather than authoritative. We NEVER drop results here (recall-first);
/// this only annotates them.
///
/// This heuristic predates the Tier-2 reranker (v3.4.7) and is now the
/// **no-reranker fallback only**: it fires solely inside the unreranked branch
/// of [`rerank_unreranked_hint`], when a query genuinely has no rerank score to
/// band on. Once every hit carries a `rerank_score`, [`rerank_confidence_band`]
/// supersedes this per-hit.
fn vec_confidence_hint(top_score: f64) -> Option<String> {
    if top_score >= VEC_CONFIDENT {
        None
    } else if top_score >= VEC_AMBIGUOUS {
        Some(
            "low-confidence semantic match — results may not be relevant; \
             verify or try `search search` (keyword)"
                .to_string(),
        )
    } else {
        Some("no strong semantic match — showing nearest results, likely not relevant".to_string())
    }
}

/// PROVISIONAL confidence bands from a reranked hit's calibrated
/// `rerank_score` — Task 9 recalibrates these against real query/passage
/// pairs once the reranker model is live in production; treat the thresholds
/// as placeholders, not tuned values.
///
/// The low band edge IS the engine's own rerank gate default — a hit this
/// weak only survives via the never-empty floor, so `RERANK_NO_MATCH_BAND`
/// references [`onebrain_search::engine::DEFAULT_RERANK_MIN_SCORE`] directly
/// rather than duplicating its value as a literal (they must never drift
/// apart). `0.60` (`RERANK_POSSIBLE_BAND`) has no such counterpart yet — it
/// stays a standalone provisional tunable pending Track C.
///
/// `< DEFAULT_RERANK_MIN_SCORE` → "no strong match"; `DEFAULT_RERANK_MIN_SCORE..0.60`
/// → "possible match"; `> 0.60` → confident, no label.
const RERANK_NO_MATCH_BAND: f32 = onebrain_search::engine::DEFAULT_RERANK_MIN_SCORE;
const RERANK_POSSIBLE_BAND: f32 = 0.60;

/// Confidence label for one reranked hit's `rerank_score`, or `None` when the
/// score is confidently above [`RERANK_POSSIBLE_BAND`] (no label needed).
fn rerank_confidence_band(score: f32) -> Option<&'static str> {
    if score < RERANK_NO_MATCH_BAND {
        Some("no strong match")
    } else if score <= RERANK_POSSIBLE_BAND {
        Some("possible match")
    } else {
        None
    }
}

/// One-line, never-silent hint for a semantic verb's (`query` / `vsearch`)
/// result set when NONE of its hits carry a `rerank_score` — the reranker was
/// skipped (model not downloaded, load failure) rather than turned off.
/// Distinct from a genuine empty result (handled by `index_hint_for`) and from
/// a partially-reranked set (the fused tail beyond `min_candidates` — those
/// hits simply render with no band, no separate hint line).
///
/// `reranker_enabled: false` is an explicit user choice, not a degraded state —
/// design calls for NO hint in that case (bands can't appear either, since
/// scores stay `None`), so this returns `None` immediately rather than the
/// misleading "model not downloaded" line.
///
/// `vsearch` additionally folds in the pre-reranker cosine heuristic
/// ([`vec_confidence_hint`]) as extra detail, since a bare cosine score is
/// still a meaningful (if noisier) signal for that verb; `query`'s fused RRF
/// score has no comparable interpretation, so it gets the generic line alone.
fn rerank_unreranked_hint(
    hits: &[Hit],
    vec_top_score: Option<f64>,
    reranker_enabled: bool,
) -> Option<String> {
    if !reranker_enabled || hits.is_empty() || hits.iter().any(|h| h.rerank_score.is_some()) {
        return None;
    }
    let base = "unreranked (reranker model not downloaded — run `onebrain search reindex`)";
    match vec_top_score.and_then(vec_confidence_hint) {
        Some(cosine_hint) => Some(format!("{base} · {cosine_hint}")),
        None => Some(base.to_string()),
    }
}

/// `onebrain search vsearch` — vector-only semantic search. Also embeds the
/// query text (model-download point on first use).
///
/// NOT daemon-routable: the warm daemon's `/api/vault/search` exposes only
/// `lex` + `hybrid` (no vector-only mode), so vsearch always opens the engine
/// directly. When a daemon is holding the engine (an active `onebrain mcp`
/// session), this therefore degrades to T1's honest `E_ENGINE_BUSY` rather than
/// silently returning hybrid results under a different ranking — use `query`
/// (hybrid, daemon-routable) if you need results while a session is live. A
/// dedicated vec-only daemon endpoint is a follow-up (out of scope for 2c).
pub fn run_vsearch(
    vault_flag: Option<PathBuf>,
    mode: &OutputMode,
    args: &SearchQueryArgs,
) -> Result<()> {
    let (mut engine, resolved) = open_engine(vault_flag)?;
    let raw_min = apply_rerank_overrides(&mut engine, args);
    let vault_info = crate::vault_ctx::info_from(&resolved);
    let hits = apply_min_score(engine.vector_search(&args.text, args.top_k)?, raw_min);
    // Never-silent: an empty result explains itself (empty/stale index).
    // A non-empty, UNRERANKED result still carries a confidence signal — the
    // pre-reranker cosine heuristic — as a fallback inside
    // `rerank_unreranked_hint`; a reranked result gets per-hit bands in the
    // text renderer instead (see `rerank_confidence_band`), no separate hint.
    let hint = match hits.first() {
        None => index_hint_for(&engine, &resolved),
        Some(top) => rerank_unreranked_hint(&hits, Some(top.score), engine.rerank_enabled()),
    };
    emit_hits(
        "search.vec",
        &resolved,
        vault_info,
        hits,
        hint,
        args.opt_level.as_deref(),
        mode,
    )
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
    let lex = LexIndex::open(&index_artifact_path(&cache_dir, "tantivy"))
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
                // Pure-lex `search` never reranks — untouched per Task 6 scope.
                rerank_score: None,
            }
        })
        .collect();

    // No engine here (deliberately — no embedder), so no index-state probe.
    emit_hits_data(
        "search.lex",
        &resolved,
        vault_info,
        hits,
        None,
        args.opt_level.as_deref(),
        mode,
    )
}

/// Run a search through the warm daemon's `/api/vault/search` (mode `hybrid` /
/// `lex`) and emit the same envelope the direct path does. The daemon is the
/// sole redb owner, so this never hits the single-writer lock.
///
/// The daemon caps top-k (~20) and returns `{path, score, title, snippet}`
/// rows; `--top-k` / `--min-score` are applied client-side over that set so
/// the flags behave the same as the direct path (within the daemon's cap). On
/// an empty result, the index hint is derived from the daemon's
/// `/api/internal/status` (empty / behind), matching the direct path's
/// `index_hint_for`.
///
/// Semantic-only: the lex-only build's `run_query` degrades to `run_lex`
/// (a standalone `LexIndex`, no redb → no daemon contention), so the daemon
/// query path is never reached there.
#[cfg(feature = "semantic")]
fn run_query_via_daemon(
    handle: &DaemonHandle,
    resolved: &onebrain_core::ResolvedVault,
    daemon_mode: &str,
    args: &SearchQueryArgs,
    mode: &OutputMode,
) -> Result<()> {
    let vault_info = crate::vault_ctx::info_from(resolved);
    let resp = handle
        .search(
            &args.text,
            daemon_mode,
            Some(args.top_k),
            args.min_candidates,
        )
        .map_err(|e| map_daemon_error(e, "route search through warm daemon"))?;

    let mut hits = daemon_hits(&resp);

    // Same rerank-enabled/gate knobs the direct path reads via `open_engine` —
    // config is read-only here (no engine/embedder is opened on this path),
    // so this can't trigger a model download.
    let (rerank_enabled, config_gate) = onebrain_core::load_vault_config(&resolved.root)
        .map(|cfg| {
            let s = rerank_settings_from_config(&cfg.search.reranker);
            (s.enabled, s.min_score)
        })
        .unwrap_or((true, onebrain_search::engine::DEFAULT_RERANK_MIN_SCORE));

    // `--min-score` unification (mirrors the direct path): when the daemon
    // reranked (hits carry `rerank_score`), filter by that calibrated 0–1
    // score; otherwise fall back to the raw fused score. NOTE: the daemon
    // already applied its configured `min_score` gate before returning, so
    // on this path `--min-score` can only TIGHTEN past that config floor, not
    // loosen below it — a per-query API override is a v3.4.8 follow-up.
    if let Some(min) = args.min_score {
        let reranked = rerank_enabled && hits.iter().any(|h| h.rerank_score.is_some());
        if reranked {
            let minf = min as f32;
            // Never-silent: the daemon already gated at its config `min_score`
            // before returning, so a LOOSER `--min-score` here can't restore
            // dropped hits — only tighten. Say so rather than let the flag
            // appear to do nothing. (The direct, non-daemon path can loosen.)
            if minf < config_gate {
                eprintln!(
                    "note: this vault's live daemon already gated at min_score {config_gate:.2}; \
                     --min-score {min:.2} only tightens further on the daemon path (run with \
                     ONEBRAIN_NO_DAEMON=1 for a direct query that can loosen it)"
                );
            }
            hits.retain(|h| h.rerank_score.map(|rs| rs >= minf).unwrap_or(false));
        } else {
            hits.retain(|h| h.score >= min);
        }
    }
    hits.truncate(args.top_k);

    let hint = if hits.is_empty() {
        daemon_index_hint(handle)
    } else {
        // Mirror the direct `query` path's generic (no cosine-top-score)
        // unreranked hint — the daemon's fused score has no cosine
        // interpretation either.
        rerank_unreranked_hint_data(&hits, rerank_enabled)
    };
    let command = if daemon_mode == "hybrid" {
        "search.query"
    } else {
        "search.lex"
    };
    emit_hits_data(
        command,
        resolved,
        vault_info,
        hits,
        hint,
        args.opt_level.as_deref(),
        mode,
    )
}

/// [`rerank_unreranked_hint`]'s counterpart for the daemon path, which works
/// in terms of `HitData` (already mapped from the daemon's JSON) rather than
/// the engine's `Hit`. Same generic-line-only shape as the direct `query`
/// path: the daemon's fused score has no cosine interpretation to fold in.
#[cfg(feature = "semantic")]
fn rerank_unreranked_hint_data(hits: &[HitData], reranker_enabled: bool) -> Option<String> {
    if !reranker_enabled || hits.is_empty() || hits.iter().any(|h| h.rerank_score.is_some()) {
        return None;
    }
    Some("unreranked (reranker model not downloaded — run `onebrain search reindex`)".to_string())
}

/// Map the daemon `/api/vault/search` response (`{hits:[{path,score,title,
/// snippet,rerank_score?}], mode}`) into the CLI's `HitData`. `chunk_id`/
/// `doc_path` both take the doc path (the daemon collapses per-chunk hits to
/// one row per doc); `heading_path` takes the title UNLESS it equals the file
/// stem (which the daemon substitutes when a doc has no heading) — so the
/// text render doesn't print `note.md › note`. A malformed / missing `hits`
/// array maps to empty.
///
/// `rerank_score` is read the same lenient way `mcp.rs`'s
/// `parse_daemon_search_hits` reads it: `get(...).and_then(as_f64)`, so a
/// missing key (old daemon binary predating the wire field) or a non-numeric
/// value (contract break) both degrade to `None` rather than erroring the
/// whole search — an unreranked hit is a normal, expected shape.
#[cfg(feature = "semantic")]
fn daemon_hits(resp: &serde_json::Value) -> Vec<HitData> {
    let Some(arr) = resp.get("hits").and_then(|h| h.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|h| {
            let path = h.get("path")?.as_str()?.to_string();
            let score = h.get("score").and_then(|s| s.as_f64()).unwrap_or(0.0);
            let title = h
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            let heading_path = if title.is_empty() || title == file_stem(&path) {
                String::new()
            } else {
                title
            };
            let snippet = h
                .get("snippet")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            let rerank_score = h
                .get("rerank_score")
                .and_then(|v| v.as_f64())
                .map(|s| s as f32);
            Some(HitData {
                chunk_id: path.clone(),
                doc_path: path,
                heading_path,
                score,
                snippet,
                rerank_score,
            })
        })
        .collect()
}

/// The `title`-vs-stem test's stem: last path segment with a trailing `.md`
/// stripped — mirrors the daemon's `title_from_path` so a headingless hit's
/// title (the stem) collapses to an empty `heading_path`.
#[cfg(feature = "semantic")]
fn file_stem(path: &str) -> String {
    let file = path.rsplit('/').next().unwrap_or(path);
    file.strip_suffix(".md").unwrap_or(file).to_string()
}

/// Build the empty-result index hint from the daemon's `/api/internal/status`,
/// mirroring the direct path's [`index_hint_for`]. Best-effort: any probe
/// failure degrades to no hint rather than failing the search.
#[cfg(feature = "semantic")]
fn daemon_index_hint(handle: &DaemonHandle) -> Option<String> {
    let st = handle.status().ok()?;
    if st.get("doc_count").and_then(|c| c.as_u64()) == Some(0) {
        return Some("index is empty — run `onebrain search reindex` first".to_string());
    }
    let pending = st
        .get("pending_total")
        .and_then(|p| p.as_u64())
        .unwrap_or(0);
    (pending > 0).then(|| {
        format!(
            "index is behind — {pending} doc(s) not yet indexed · run `onebrain search reindex`"
        )
    })
}

/// Apply the optional `--min-score` floor to engine hits.
fn apply_min_score(mut hits: Vec<Hit>, min_score: Option<f64>) -> Vec<Hit> {
    if let Some(min) = min_score {
        hits.retain(|h| h.score >= min);
    }
    hits
}

/// Thread the per-query CLI overrides (`--min-candidates`, `--min-score`) into
/// the engine's config-derived `RerankSettings` (already installed by
/// `open_engine`) and decide how `--min-score` is interpreted.
///
/// `--min-candidates` is a FLOOR (`Engine::apply_rerank` reranks
/// `max(min_candidates, top_k)`), so overriding it never shrinks the reranked
/// window below `--top-k`.
///
/// `--min-score` is UNIFIED with the rerank gate: when reranking is **active**
/// (enabled + model available), the flag overrides the rerank `min_score` gate
/// so it filters the calibrated 0–1 `rerank_score` — the confidence number a
/// user can actually reason about — and this returns `None` so the caller does
/// NOT also filter the raw retrieval score. When reranking is **inactive**
/// (disabled / model not downloaded / lex-only), the flag keeps its legacy
/// meaning and this returns `Some(min)` for the caller to apply to the raw
/// retrieval score (cosine for `vsearch`, RRF for `query`). In a lex-only
/// build `rerank_active()` is always false, so `--min-score` keeps its legacy
/// raw-score meaning there.
fn apply_rerank_overrides(
    engine: &mut onebrain_search::engine::Engine,
    args: &SearchQueryArgs,
) -> Option<f64> {
    if let Some(min_candidates) = args.min_candidates {
        engine.set_rerank_min_candidates(min_candidates);
    }
    match args.min_score {
        Some(min) if engine.rerank_active() => {
            engine.set_rerank_min_score(min as f32);
            None
        }
        other => other,
    }
}

fn emit_hits(
    command: &str,
    resolved: &ResolvedVault,
    vault_info: crate::output::VaultInfo,
    hits: Vec<Hit>,
    index_hint: Option<String>,
    opt_level: Option<&str>,
    mode: &OutputMode,
) -> Result<()> {
    let hits: Vec<HitData> = hits.into_iter().map(HitData::from).collect();
    emit_hits_data(
        command, resolved, vault_info, hits, index_hint, opt_level, mode,
    )
}

/// Emit a `HitData` result set, shaping it through the token funnel
/// (`Surface::CliSearch`) FIRST when the output is structured (`--output
/// json`/`yaml`) — the agent-facing path. Human TTY text is emitted unchanged
/// (design §1: "human TTY output is untouched this release"), and no gain event
/// is recorded for it. Shared by every CLI search verb so the shaping and
/// metering land identically on the direct, lex, and daemon-routed paths.
fn emit_hits_data(
    command: &str,
    resolved: &ResolvedVault,
    vault_info: crate::output::VaultInfo,
    hits: Vec<HitData>,
    index_hint: Option<String>,
    opt_level: Option<&str>,
    mode: &OutputMode,
) -> Result<()> {
    let hits = if mode.is_structured() {
        shape_cli_hits(resolved, hits, opt_level)
    } else {
        hits
    };
    let envelope = Envelope::ok(
        command,
        Some(vault_info),
        SearchHitsData { hits, index_hint },
    );
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

/// Shape a CLI hit list through the funnel: map `HitData` to the canonical
/// `doc_path`+`text` hit JSON the query transforms understand (design §1),
/// funnel it (`CliSearch`), and map back. Records one gain event. Level
/// resolves per-call `--opt-level` > config > default.
fn shape_cli_hits(
    resolved: &ResolvedVault,
    hits: Vec<HitData>,
    opt_level: Option<&str>,
) -> Vec<HitData> {
    let cfg = onebrain_core::load_vault_config(&resolved.root)
        .map(|c| c.token_optimization)
        .unwrap_or_default();
    let level = token_runner::resolve_level(opt_level, &cfg).unwrap_or(cfg.level);
    let ctx = token_runner::ctx_for(level, &cfg, None, false, None);
    let canonical: Vec<serde_json::Value> = hits.iter().map(hitdata_to_canonical).collect();
    let shaped = token_runner::shape_query_hits(resolved, Surface::CliSearch, canonical, &ctx);
    shaped.into_iter().filter_map(canonical_from).collect()
}

/// `HitData` → canonical hit JSON (`snippet`→`text`; `doc_path` already
/// matches). Other fields ride along untouched.
fn hitdata_to_canonical(h: &HitData) -> serde_json::Value {
    let mut v = serde_json::to_value(h).unwrap_or(serde_json::Value::Null);
    if let Some(obj) = v.as_object_mut() {
        if let Some(snippet) = obj.remove("snippet") {
            obj.insert("text".to_string(), snippet);
        }
    }
    v
}

/// Canonical hit JSON → `HitData` (`text`→`snippet`, absent after `disclosure`
/// → empty snippet, matching the required `String` field). A hit that fails to
/// round-trip is dropped rather than corrupting the result set.
fn canonical_from(mut v: serde_json::Value) -> Option<HitData> {
    if let Some(obj) = v.as_object_mut() {
        let text = obj
            .remove("text")
            .unwrap_or_else(|| serde_json::Value::String(String::new()));
        obj.insert("snippet".to_string(), text);
    }
    serde_json::from_value::<HitData>(v).ok()
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
        // Reranked hits carry a calibrated relevance band alongside the raw
        // fused/cosine score — PROVISIONAL thresholds (Task 9 recalibrates).
        if let Some(rerank_score) = h.rerank_score {
            if let Some(band) = rerank_confidence_band(rerank_score) {
                block.push_str(&format!("  ⚠️ {band} ({rerank_score:.3})"));
            }
        }
        if !h.snippet.is_empty() {
            block.push_str(&format!("\n     {}", h.snippet));
        }
        blocks.push(block);
    }
    // Blank line between hits so each result reads as its own block.
    let mut out = blocks.join("\n\n");
    // Never-silent: surface a confidence/index hint even when we DID return
    // hits (e.g. a low-confidence vsearch), so weak results read as honest.
    if let Some(h) = &d.index_hint {
        out.push_str(&format!("\n\nℹ️  {h}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vec_confidence_hint_labels_weak_results_but_not_strong() {
        assert_eq!(
            vec_confidence_hint(0.90),
            None,
            "a clear match (≥0.86) needs no hint"
        );
        assert!(
            vec_confidence_hint(0.83).is_some(),
            "the ambiguous band (0.80–0.86) gets a low-confidence hint"
        );
        let weak = vec_confidence_hint(0.75).expect("below 0.80 must still return a hint");
        assert!(
            weak.contains("no strong"),
            "below-band hint names it plainly: {weak}"
        );
    }

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

    /// Test-only `HitData` builder: fills every field with a harmless default
    /// (`rerank_score: None`) so each test only spells out the fields it
    /// actually cares about.
    fn hit(doc_path: &str, score: f64) -> HitData {
        HitData {
            chunk_id: format!("{doc_path}#0"),
            doc_path: doc_path.to_string(),
            heading_path: String::new(),
            score,
            snippet: String::new(),
            rerank_score: None,
        }
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
            rerank_score: None,
        }]));
        assert!(s.contains("📄  1. a.md › Intro  (0.500)"));
        assert!(s.contains("hello world"));
    }

    #[test]
    fn text_surfaces_hint_on_weak_but_present_hits() {
        // Never-silent: a low-confidence vsearch still shows its hits AND the
        // confidence hint (the old renderer showed hints only on empty results).
        let e = Envelope::ok(
            "search.vec",
            None,
            SearchHitsData {
                hits: vec![HitData {
                    chunk_id: "a.md#0".into(),
                    doc_path: "a.md".into(),
                    heading_path: String::new(),
                    score: 0.83,
                    snippet: String::new(),
                    rerank_score: None,
                }],
                index_hint: Some("⚠️  low-confidence semantic match".into()),
            },
        );
        let s = render_text(&e);
        assert!(s.contains("a.md"), "shows the hit: {s}");
        assert!(
            s.contains("low-confidence"),
            "surfaces the hint on non-empty results: {s}"
        );
    }

    #[test]
    fn text_renders_hit_without_heading() {
        let s = render_text(&env(vec![HitData {
            chunk_id: "a.md#0".into(),
            doc_path: "a.md".into(),
            heading_path: String::new(),
            score: 0.5,
            snippet: String::new(),
            rerank_score: None,
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
                rerank_score: None,
            },
            HitData {
                chunk_id: "b.md#0".into(),
                doc_path: "b.md".into(),
                heading_path: String::new(),
                score: 0.5,
                snippet: "second".into(),
                rerank_score: None,
            },
        ]));
        assert!(s.contains("📄  1. a.md"));
        assert!(s.contains("📄  2. b.md"));
        // Blank line separates the two hit blocks.
        assert!(s.contains("first\n\n📄  2."));
    }

    // ── Rerank confidence bands + hints (v3.4.7 Task 6) ────────────────────

    #[test]
    fn rerank_confidence_band_thresholds() {
        // < 0.30 → "no strong match".
        assert_eq!(rerank_confidence_band(0.0), Some("no strong match"));
        assert_eq!(rerank_confidence_band(0.29), Some("no strong match"));
        // 0.30..=0.60 → "possible match".
        assert_eq!(rerank_confidence_band(0.30), Some("possible match"));
        assert_eq!(rerank_confidence_band(0.45), Some("possible match"));
        assert_eq!(rerank_confidence_band(0.60), Some("possible match"));
        // > 0.60 → confident, no label.
        assert_eq!(rerank_confidence_band(0.61), None);
        assert_eq!(rerank_confidence_band(0.95), None);
    }

    #[test]
    fn text_renders_band_for_no_strong_match() {
        let mut h = hit("a.md", 0.5);
        h.rerank_score = Some(0.10);
        let s = render_text(&env(vec![h]));
        assert!(s.contains("no strong match"), "{s}");
        assert!(s.contains("(0.100)"), "band shows the rerank score: {s}");
    }

    #[test]
    fn text_renders_band_for_possible_match() {
        let mut h = hit("a.md", 0.5);
        h.rerank_score = Some(0.45);
        let s = render_text(&env(vec![h]));
        assert!(s.contains("possible match"), "{s}");
    }

    #[test]
    fn text_renders_no_band_for_confident_rerank_score() {
        let mut h = hit("a.md", 0.5);
        h.rerank_score = Some(0.95);
        let s = render_text(&env(vec![h]));
        assert!(
            !s.contains("no strong match") && !s.contains("possible match"),
            "a confident (>0.60) rerank score gets no band label: {s}"
        );
    }

    #[test]
    fn never_empty_case_renders_no_strong_match_label() {
        // The engine's never-empty floor (RERANK_NO_MATCH_KEEP) keeps hits
        // even when every candidate is gated out — those survivors carry a
        // low rerank_score, so the CLI must still show the honest band
        // rather than rendering them as if they were confident matches.
        let mut h = hit("only-survivor.md", 0.2);
        h.rerank_score = Some(0.05);
        let s = render_text(&env(vec![h]));
        assert!(s.contains("no strong match"), "{s}");
    }

    #[test]
    fn rerank_unreranked_hint_none_when_any_hit_has_rerank_score() {
        // Mixed reranked-head + unreranked-tail (fused tail beyond
        // `min_candidates`) must NOT trigger the unreranked hint — only a set
        // with NO rerank score anywhere counts as "the reranker was skipped".
        let mut reranked = engine_hit("a.md", 0.9);
        reranked.rerank_score = Some(0.8);
        let unreranked_tail = engine_hit("b.md", 0.5);
        assert_eq!(
            rerank_unreranked_hint(&[reranked, unreranked_tail], None, true),
            None
        );
    }

    #[test]
    fn rerank_unreranked_hint_none_on_empty_hits() {
        assert_eq!(rerank_unreranked_hint(&[], Some(0.9), true), None);
    }

    #[test]
    fn rerank_unreranked_hint_generic_line_for_query_verb() {
        // `query` (hybrid) has no cosine top score to fold in.
        let hits = [engine_hit("a.md", 0.9)];
        let hint = rerank_unreranked_hint(&hits, None, true).expect("unreranked hits need a hint");
        assert!(hint.contains("unreranked"), "{hint}");
        assert!(hint.contains("reranker model not downloaded"), "{hint}");
        assert!(hint.contains("onebrain search reindex"), "{hint}");
    }

    #[test]
    fn rerank_unreranked_hint_folds_in_cosine_heuristic_for_vsearch() {
        // vsearch still has a meaningful cosine top score, so the unreranked
        // fallback keeps the old heuristic alongside the generic line.
        let hits = [engine_hit("a.md", 0.9)];
        let hint =
            rerank_unreranked_hint(&hits, Some(0.75), true).expect("unreranked hits need a hint");
        assert!(hint.contains("unreranked"), "{hint}");
        assert!(
            hint.contains("no strong semantic match"),
            "cosine fallback folded in: {hint}"
        );
    }

    #[test]
    fn rerank_unreranked_hint_confident_cosine_omits_heuristic_detail() {
        // A confident cosine top score (≥0.86) has nothing extra to say, so
        // only the generic unreranked line appears.
        let hits = [engine_hit("a.md", 0.9)];
        let hint = rerank_unreranked_hint(&hits, Some(0.90), true).expect("still unreranked");
        assert_eq!(
            hint,
            "unreranked (reranker model not downloaded — run `onebrain search reindex`)"
        );
    }

    #[test]
    fn rerank_unreranked_hint_none_when_reranker_disabled() {
        // `enabled: false` is an explicit user choice, not a degraded state —
        // design calls for NO hint (bands can't appear either, since scores
        // stay `None`), never the misleading "model not downloaded" line.
        let hits = [engine_hit("a.md", 0.9)];
        assert_eq!(rerank_unreranked_hint(&hits, None, false), None);
        assert_eq!(rerank_unreranked_hint(&hits, Some(0.5), false), None);
    }

    /// Minimal `onebrain_search::engine::Hit` builder for the pre-`HitData`
    /// hint-computation tests above.
    fn engine_hit(doc_path: &str, score: f64) -> Hit {
        Hit {
            chunk_id: format!("{doc_path}#0"),
            doc_path: doc_path.to_string(),
            heading_path: String::new(),
            score,
            snippet: String::new(),
            rerank_score: None,
        }
    }

    // ── Daemon response mapping (Track 2c) ─────────────────────────────────

    #[cfg(feature = "semantic")]
    #[test]
    fn file_stem_strips_dirs_and_md() {
        assert_eq!(file_stem("01-projects/oma/x.md"), "x");
        assert_eq!(file_stem("note.md"), "note");
        assert_eq!(file_stem("no-ext"), "no-ext");
    }

    #[cfg(feature = "semantic")]
    #[test]
    fn daemon_hits_maps_fields_and_collapses_stem_title() {
        let resp = serde_json::json!({
            "mode": "hybrid",
            "hits": [
                // Title differs from the stem → kept as heading_path.
                {"path": "a/b.md", "score": 0.9, "title": "Intro", "snippet": "hello"},
                // Title equals the file stem (headingless) → heading_path empty.
                {"path": "c.md", "score": 0.4, "title": "c", "snippet": ""},
            ]
        });
        let hits = daemon_hits(&resp);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].doc_path, "a/b.md");
        assert_eq!(hits[0].chunk_id, "a/b.md");
        assert_eq!(hits[0].heading_path, "Intro");
        assert_eq!(hits[0].score, 0.9);
        assert_eq!(hits[0].snippet, "hello");
        // Stem-equal title collapses so the render never prints `c.md › c`.
        assert_eq!(hits[1].heading_path, "");
    }

    #[cfg(feature = "semantic")]
    #[test]
    fn daemon_hits_empty_on_missing_or_malformed_array() {
        assert!(daemon_hits(&serde_json::json!({})).is_empty());
        assert!(daemon_hits(&serde_json::json!({"hits": "nope"})).is_empty());
        // A row missing `path` is skipped (filter_map), not panicked on.
        let resp = serde_json::json!({"hits": [{"score": 1.0}]});
        assert!(daemon_hits(&resp).is_empty());
    }

    #[cfg(feature = "semantic")]
    #[test]
    fn daemon_hits_parses_rerank_score_when_present() {
        let resp = serde_json::json!({
            "hits": [
                {"path": "a.md", "score": 0.9, "title": "a", "snippet": "", "rerank_score": 0.75},
            ]
        });
        let hits = daemon_hits(&resp);
        assert_eq!(hits[0].rerank_score, Some(0.75));
    }

    #[cfg(feature = "semantic")]
    #[test]
    fn daemon_hits_rerank_score_none_when_field_absent_old_daemon_compat() {
        // Old daemon binaries predate the `rerank_score` wire field entirely —
        // the same shape as a hit the daemon chose not to rerank.
        let resp = serde_json::json!({
            "hits": [{"path": "a.md", "score": 0.9, "title": "a", "snippet": ""}]
        });
        let hits = daemon_hits(&resp);
        assert_eq!(hits[0].rerank_score, None);
    }

    #[cfg(feature = "semantic")]
    #[test]
    fn daemon_hits_rerank_score_none_when_non_numeric_contract_break() {
        let resp = serde_json::json!({
            "hits": [{"path": "a.md", "score": 0.9, "title": "a", "snippet": "", "rerank_score": "bogus"}]
        });
        let hits = daemon_hits(&resp);
        assert_eq!(hits[0].rerank_score, None);
    }

    #[cfg(feature = "semantic")]
    #[test]
    fn rerank_unreranked_hint_data_none_when_any_hit_has_rerank_score() {
        let mut reranked = hit("a.md", 0.9);
        reranked.rerank_score = Some(0.8);
        let unreranked_tail = hit("b.md", 0.5);
        assert_eq!(
            rerank_unreranked_hint_data(&[reranked, unreranked_tail], true),
            None
        );
    }

    #[cfg(feature = "semantic")]
    #[test]
    fn rerank_unreranked_hint_data_fires_when_no_hit_has_rerank_score() {
        let hits = [hit("a.md", 0.9)];
        let hint = rerank_unreranked_hint_data(&hits, true).expect("unreranked hits need a hint");
        assert!(hint.contains("unreranked"), "{hint}");
        assert!(hint.contains("reranker model not downloaded"), "{hint}");
    }

    #[cfg(feature = "semantic")]
    #[test]
    fn rerank_unreranked_hint_data_none_when_disabled() {
        let hits = [hit("a.md", 0.9)];
        assert_eq!(rerank_unreranked_hint_data(&hits, false), None);
    }

    #[cfg(feature = "semantic")]
    #[test]
    fn rerank_unreranked_hint_data_none_on_empty_hits() {
        assert_eq!(rerank_unreranked_hint_data(&[], true), None);
    }
}
