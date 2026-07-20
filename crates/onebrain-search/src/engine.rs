//! Ties `chunk` + `embed` + `vector` + `lex` + `hybrid` together into a single
//! synchronous search engine: index a document, remove a document, and run a
//! hybrid (lex + vector, RRF-fused) query.
//!
//! ## Indexing scope
//!
//! Only Markdown (`*.md`) files are indexed. The vault walk
//! ([`walk_markdown_files`]) collects nothing but `*.md` files — any other file
//! type in the vault is never read, chunked, embedded, or stored. Hidden dirs
//! and `node_modules` are always skipped, plus the vault's configured
//! `search.exclude` patterns.
//!
//! ## On-disk layout (under `cache_dir`)
//! The collection cache root is split into `index/` (search artifacts) and
//! `models/` (the hf-hub embedding/reranker download cache) — see
//! [`crate::layout::CollectionLayout`], which resolves each artifact with a
//! legacy-flat-root fallback and migrates the flat layout into the split one
//! eagerly on open.
//! - `<cache_dir>/index/tantivy/` — [`crate::lex::LexIndex`] (BM25 lexical index).
//! - `<cache_dir>/index/vectors/` — [`crate::vector::VectorStore`] (flat mmap vector store).
//! - `<cache_dir>/index/engine.redb` — chunk metadata (see [`ChunkMeta`]) and the
//!   per-doc chunk-id list, both keyed by strings and serialized with
//!   `serde_json`. Neither `lex` nor `vector` stores the chunk's text or
//!   heading path, so this database is the only place [`Engine::get`] and
//!   [`Hit`] snippets can source that data from.

use std::cell::OnceCell;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::chunk::{chunk_markdown, Chunk};
use crate::embed::{self, Embed};
use crate::hybrid::rrf_fuse;
use crate::layout::CollectionLayout;
use crate::lex::{self, LexIndex};
use crate::rerank::{self, Rerank};
use crate::vector::VectorStore;

const CHUNK_META: TableDefinition<&str, &str> = TableDefinition::new("chunk_meta");
const DOC_CHUNKS: TableDefinition<&str, &str> = TableDefinition::new("doc_chunks");
const DOC_HASHES: TableDefinition<&str, &str> = TableDefinition::new("doc_hashes");
/// Content hash stored by a **lex-only** reindex ([`IndexMode::LexOnly`]).
/// Deliberately a separate table from `DOC_HASHES`, which means "vectors are
/// current as of this hash": a doc lex-indexed but never embedded must keep
/// reporting as pending in [`Engine::status`] so a later full/pending embed
/// pass finds it. See [`Engine::effective_lex_hash`] for how the two tables
/// combine on read.
const LEX_HASHES: TableDefinition<&str, &str> = TableDefinition::new("lex_hashes");
const ENGINE_HEADER: TableDefinition<&str, &str> = TableDefinition::new("engine_header");

const ACTIVE_MODEL_KEY: &str = "active_model";
const LAST_INDEXED_KEY: &str = "last_indexed_at";
/// Monotonic index-version counter (design §3a). Bumped in a single atomic
/// write txn at the end of EVERY reindex — full AND lex-only — so the
/// query-result memoization cache (Track 3, `onebrain-token`) can put it in
/// its key and be structurally immune to staleness: any reindex that could
/// change results also changes `generation`, so a prior memo entry can never
/// match. Unlike [`LAST_INDEXED_KEY`] (full-mode only — it means "vectors are
/// current as of"), this MUST bump on the lex-only path too, because the
/// constantly-firing PostToolUse lex-only reindex hook changes lex results
/// without ever embedding.
const GENERATION_KEY: &str = "generation";
/// Stable per-index-instance nonce (design §3a). Written ONCE when a fresh
/// `engine.redb` is created and NEVER reset by a reindex — only a brand-new
/// index (the `reindex --force` / cache-split-migration / `rm index/` wipe of
/// `engine.redb`, which drops this whole header) gets a new one. The memo
/// cache folds it into every key alongside [`GENERATION_KEY`] so a rebuilt
/// index — whose `generation` restarts at 0→1 and would otherwise COLLIDE with
/// stale memo entries written under the old index's generation 1 — instead
/// lands in a fresh key space and can never serve a pre-rebuild hit.
const INDEX_NONCE_KEY: &str = "index_nonce";

const CHUNK_MAX_TOKENS: usize = 512;
const CHUNK_OVERLAP_TOKENS: usize = 64;
const LEX_TOP_K: usize = 50;
const VEC_TOP_K: usize = 50;
const RRF_K: f64 = 60.0;
/// Cosine window for the recall-first vector cutoff ([`keep_top_cluster`]):
/// keep hits within this much of a query's best score. Measured on real e5
/// content (2026-07-06) — genuine matches cluster ~0.012–0.019 below the top,
/// so 0.02 keeps the coherent cluster while shedding the flat tail. Model-
/// agnostic (relative), replacing the old per-model absolute `vec_floor`.
const VEC_CLUSTER_WINDOW: f32 = 0.02;
const SNIPPET_MAX_CHARS: usize = 200;

/// When the rerank gate rejects every candidate, keep this many top-scored
/// hits (the CLI labels them "no strong match") instead of returning nothing —
/// ADR 0024's "never empties a non-empty result set" invariant carried into
/// the Tier-2 rerank stage.
const RERANK_NO_MATCH_KEEP: usize = 3;

/// Default rerank-score gate — **0.0, i.e. rank-only, drop nothing**.
///
/// v3.4.7 shipped 0.30, calibrated on question-shaped queries where genuine
/// matches scored 0.73-0.99 and non-matches 0.003-0.066. That calibration held
/// for the query shape it was measured on and missed the one it wasn't:
/// keyword and fragment queries -- exactly what the agent's `lex` sub-queries
/// and a `heading_path` lookup produce -- score inside the gated band even
/// when they are correct.
///
/// Measured 2026-07-19 on a real 782-doc vault, 60 probes: the 0.30 gate cut
/// heading-shaped hit@10 from 0.500 to 0.233 and body-term hit@10 from 0.733
/// to 0.500 -- **half the correct answers removed**. The previous doc comment
/// claimed a bad value here "cannot silence results" because
/// [`RERANK_NO_MATCH_KEEP`] backstops total rejection; the damage came from
/// PARTIAL rejection, which nothing backstopped, since `candidates` and
/// `top_k` are both 10 and so no fused tail remains to backfill with.
///
/// At 0.0 the cross-encoder still does its real job -- it REORDERS, putting
/// strong matches on top -- and every hit carries its `rerank_score`, which is
/// what the search cascade instructs the agent to judge confidence on
/// (`<0.30` no strong match, `0.30-0.60` possible, `>=0.60` confident). The
/// engine no longer repeats that judgement by deleting rows.
///
/// Raising `search.reranker.min_score` above 0.0 re-enables hard filtering as
/// an explicit opt-in, with [`RERANK_NO_MATCH_KEEP`] still guarding total
/// rejection.
pub const DEFAULT_RERANK_MIN_SCORE: f32 = 0.0;

/// Error message returned by embedder-backed operations (`query`,
/// `vector_search`, model switch, and the lazy embedder) in a lex-only build
/// compiled without the `semantic` feature — i.e. on a platform with no ONNX
/// Runtime prebuilt. See docs/decisions/0017-platform-tiered-semantic-search.md.
pub const SEMANTIC_UNAVAILABLE: &str =
    "semantic search isn't available in this build (no ONNX runtime for this platform)";

/// How often (in chunks) `Engine::open`'s own stderr reporting prints a line
/// while repopulating a wiped lex index. Every chunk is far too chatty on a
/// 6k-chunk vault; the `(0, total)` announcement and the final line always
/// print regardless.
const REPOPULATE_PROGRESS_EVERY: usize = 500;

/// Cheap consistency snapshot of the keyword (tantivy) index against the
/// stored chunk metadata — see [`Engine::lex_health`].
///
/// Exists because a half-finished schema migration can leave a lex index that
/// is EMPTY while every other signal reports a healthy collection: `status`
/// counts docs from redb, and `reindex` skips every doc because `lex_hashes`
/// says it is current. Comparing the two counts is the only cheap way to see
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LexHealth {
    /// Committed, non-deleted documents in the tantivy index (one per chunk).
    pub lex_docs: u64,
    /// Rows in redb's `chunk_meta` — the authoritative chunk count.
    pub chunk_meta: u64,
    /// True when a lex rebuild was started but never confirmed complete (the
    /// marker file from [`crate::lex::open_or_reset`][LexIndex::open_or_reset]
    /// is still on disk). `Engine::open` retries the rebuild whenever this is
    /// set, so seeing it from a CLI means the rebuild is failing repeatedly.
    pub rebuild_pending: bool,
}

impl LexHealth {
    /// True for the failure this type exists to catch: the collection holds
    /// chunks but the keyword index has none, so every BM25 query returns
    /// nothing while the collection looks fully indexed. Recovery is
    /// `onebrain search reindex --force`.
    ///
    /// Deliberately NOT `lex_docs == chunk_meta`: tantivy counts only
    /// committed, non-deleted docs, so a benign skew (an in-flight writer, a
    /// chunk skipped as corrupt) would otherwise be reported as breakage.
    pub fn is_dead(&self) -> bool {
        self.chunk_meta > 0 && self.lex_docs == 0
    }

    /// True for the opposite skew: the keyword index holds MORE documents than
    /// there are chunks. Unlike a shortfall, this direction is never benign.
    ///
    /// [`Self::is_dead`]'s rationale for not simply comparing `!=` covers only
    /// skew DOWNWARD — tantivy counts committed, non-deleted docs, so an
    /// in-flight writer or a chunk skipped as corrupt legitimately leaves
    /// `lex_docs < chunk_meta`. Nothing legitimate produces the reverse: extra
    /// committed documents are either duplicates of chunks that are also
    /// present (a rebuild appended onto a non-empty index — the bug
    /// [`Engine::repopulate_lex_from_meta`]'s unconditional clear now
    /// prevents) or orphans of chunks already gone from redb (a crash between
    /// `remove_doc`'s redb commit and its lex commit). Both are real damage:
    /// duplicates corrupt BM25 document frequencies and average field length
    /// so relevance decays, and orphans are dead weight that
    /// [`Engine::resolve_hits`] can only drop.
    ///
    /// Deliberately a separate predicate rather than a widening of
    /// [`Self::is_dead`]: dead means "keyword search returns nothing", which
    /// is an error; this means "keyword search returns worse results", which
    /// is a warning. Both are repaired by the same rebuild.
    pub fn has_excess_docs(&self) -> bool {
        self.lex_docs > self.chunk_meta
    }

    /// True when nothing is wrong: not [`Self::is_dead`], not
    /// [`Self::has_excess_docs`], and no rebuild left pending.
    pub fn is_healthy(&self) -> bool {
        !self.is_dead() && !self.has_excess_docs() && !self.rebuild_pending
    }
}

/// Live progress events emitted during a reindex so a caller (the CLI) can
/// render a progress bar without the engine knowing anything about terminals.
///
/// The engine is UI-free: it only reports *what* happened, never *how* to draw
/// it. See [`Engine::reindex_all_with_progress`] /
/// [`Engine::reindex_paths_with_progress`].
#[derive(Debug, Clone, PartialEq)]
pub enum ReindexProgress {
    /// Emitted exactly once, first, right after the file walk and before any
    /// doc is processed — carries the run's `total` so a UI can show
    /// `0/total` immediately instead of an empty bar while the (potentially
    /// slow) model load / first embed happens.
    Walked { total: usize },
    /// Emitted exactly once, right before the first embed call of the run —
    /// i.e. before the first doc that is actually added or updated. On a first
    /// index this is where `fastembed` downloads the model (hundreds of MB to
    /// a few GB), and where an already-downloaded model is loaded into
    /// memory (seconds to minutes for the large ones), so the CLI can
    /// announce the stall before it starts. Docs that are unchanged (no
    /// embed) never trigger this; a run with nothing to (re)embed never
    /// emits it at all.
    LoadingModel,
    /// Emitted after each doc has been processed (added / updated / unchanged /
    /// removed / failed). `done` counts docs handled so far (1-based, up to
    /// `total`); `total` is the doc count computed up front from the file walk
    /// so a percentage is possible.
    Indexing {
        done: usize,
        total: usize,
        doc_path: String,
    },
    /// Emitted at most once, at the END of a full reindex, right before the
    /// engine constructs the reranker for the first time — i.e. where the
    /// reranker model download happens (mirrors [`ReindexProgress::LoadingModel`]
    /// for the embedder). Only fires when reranking is enabled and the
    /// configured reranker model is not yet downloaded; a run where it's
    /// already present, disabled, or the model name is unknown never emits
    /// it. See [`should_fetch_reranker`].
    LoadingReranker,
}

/// Outcome counts of a [`Engine::reindex_paths`] / [`Engine::reindex_all`] run.
#[derive(Debug, Default, PartialEq)]
pub struct ReindexStats {
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    pub unchanged: usize,
    /// Docs that could not be (re)indexed because reading/indexing them
    /// failed. `reindex_all` counts these and continues instead of aborting
    /// the whole batch on the first bad file.
    pub failed: usize,
}

/// Read-only snapshot of the index for `status` reporting: how many docs are
/// indexed, when the index was last (re)built, and how far the on-disk vault
/// has drifted from the index. Computed WITHOUT constructing the embedder —
/// no model download, no embed calls.
///
/// The three `pending_*` counts are exactly the diff a reindex would act on
/// (add / update / remove), MINUS the actual indexing: a pure hash walk.
#[derive(Debug, Default, PartialEq)]
pub struct IndexStatus {
    /// Number of docs currently indexed (distinct `doc_hashes` keys).
    pub doc_count: usize,
    /// Epoch seconds of the last `reindex_all` / `reindex_paths` run, or
    /// `None` if the index has never been (re)built.
    pub last_indexed_at: Option<u64>,
    /// Docs on disk with no stored hash (would be added).
    pub pending_new: usize,
    /// Docs on disk whose content hash differs from the stored one (would be
    /// re-indexed).
    pub pending_changed: usize,
    /// Indexed docs whose file is gone from disk (would be removed).
    pub pending_removed: usize,
}

impl IndexStatus {
    /// Total pending drift across all three categories.
    pub fn pending_total(&self) -> usize {
        self.pending_new + self.pending_changed + self.pending_removed
    }
}

/// Decision for a single doc when comparing its freshly computed content
/// hash against the previously stored one. Pure and side-effect free so it
/// can be unit tested without touching disk or the redb database.
#[derive(Debug, PartialEq)]
enum HashDiff {
    /// No hash was stored yet for this doc.
    Added,
    /// A hash was stored but it differs from the current content.
    Updated,
    /// The stored hash matches the current content.
    Unchanged,
}

/// Which side-effects [`Engine::index_doc_mode`] / [`Engine::reindex_existing_doc`]
/// perform for a doc. `Full` is the long-standing behavior (lex + vector +
/// `DOC_HASHES`). `LexOnly` updates lex + chunk meta only, records into
/// `LEX_HASHES` instead of `DOC_HASHES`, and never touches the embedder —
/// see the module-level lex-only-reindex doc comment on
/// [`Engine::reindex_all_lex_only_with_progress`].
#[derive(Debug, Clone, Copy, PartialEq)]
enum IndexMode {
    Full,
    LexOnly,
}

/// Compare a stored hash (if any) against the freshly computed `current`
/// hash and classify the outcome. Pure: no I/O.
fn diff_hash(stored: Option<&str>, current: &str) -> HashDiff {
    match stored {
        None => HashDiff::Added,
        Some(prev) if prev == current => HashDiff::Unchanged,
        Some(_) => HashDiff::Updated,
    }
}

/// Hex-encoded SHA-256 of `bytes`.
fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Short, stable, hex collection-hash for a vault's absolute path: the first
/// 6 hex chars of sha256(path bytes). Deterministic per path, so a vault
/// always maps to the same auto-generated collection name. Exposed so the CLI
/// can derive a default collection name (`<dir>-<hash>`) without duplicating
/// the sha2 hashing, and unit-tested here alongside the other hash helpers.
pub fn short_path_hash(path: &Path) -> String {
    let full = hash_bytes(path.as_os_str().to_string_lossy().as_bytes());
    full.chars().take(6).collect()
}

/// Derive `file_path`'s path relative to `vault_root`, using forward
/// slashes regardless of platform, for use as a stable `doc_path` /
/// `doc_hashes` key. Returns `None` if `file_path` is not under
/// `vault_root`. Pure: no I/O (works on the path strings only).
fn vault_relative_path(vault_root: &Path, file_path: &Path) -> Option<String> {
    let rel = file_path.strip_prefix(vault_root).ok()?;
    let components: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if components.is_empty() {
        return None;
    }
    Some(components.join("/"))
}

/// Confine ONE caller-supplied reindex `doc_path` to `vault_root` — the
/// **engine-level** defense-in-depth that guards the DIRECT reindex path
/// (`ONEBRAIN_NO_DAEMON=1`, daemon-unavailable fallback, plain
/// `onebrain search reindex <paths>`), which reaches
/// [`Engine::reindex_paths_with_progress`] without ever passing through the
/// HTTP-layer `confine_reindex_path` in `onebrain-cli`'s `server`.
///
/// SECURITY (#175): without this, [`reindex_paths_with_progress_inner`] does
/// `vault_root.join(doc_path)` + `std::fs::read` on the raw caller string, so
/// `"../../../../etc/passwd"` or an absolute path escapes the vault and indexes
/// an arbitrary file. Mirrors the CLI HTTP guard's checks so every reindex
/// caller — HTTP or direct — is confined:
///
/// 1. reject absolute paths (and, on Windows, a drive/root prefix),
/// 2. reject any `..` / root component lexically (fail before any syscall),
/// 3. reject an interior NUL byte (can't name a real path; malformed/hostile),
/// 4. canonical-prefix check: the deepest EXISTING ancestor of the joined path
///    must canonicalize to somewhere under the canonicalized vault root — this
///    catches a symlink component escaping the vault. A reindex target MAY be
///    absent (that's how a removed doc is expressed), so a non-existent tail is
///    fine: its `..`-free components (guard 2) are read/removed in place and
///    cannot climb out.
///
/// `Ok(())` = safe to index. `Err(reason)` names why it was rejected; the caller
/// surfaces "…: {reason}" and skips the path (never reads it).
fn confine_reindex_doc_path(vault_root: &Path, doc_path: &str) -> std::result::Result<(), String> {
    use std::path::Component;

    if doc_path.is_empty() {
        return Err("path is empty".to_string());
    }
    if doc_path.contains('\0') {
        return Err("path contains an interior NUL byte".to_string());
    }

    let rel_path = Path::new(doc_path);
    if rel_path.is_absolute() {
        return Err(format!(
            "path is outside the vault (must be relative to the vault root): {doc_path}"
        ));
    }
    for comp in rel_path.components() {
        match comp {
            Component::ParentDir => {
                return Err(format!(
                    "path is outside the vault (`..` not allowed): {doc_path}"
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "path is outside the vault (must be relative to the vault root): {doc_path}"
                ));
            }
            // Normal / CurDir are fine.
            _ => {}
        }
    }

    // Canonical-prefix check — catches a symlink component escaping the vault.
    // The vault root existed when the engine opened it; a canonicalize failure
    // here is a real environment fault, surfaced as a rejection (fail closed).
    let canonical_root = vault_root
        .canonicalize()
        .map_err(|e| format!("could not resolve vault root: {e}"))?;
    let joined = canonical_root.join(rel_path);

    // The target may not exist yet (a removed doc), so we can't canonicalize it
    // directly. Canonicalize the DEEPEST EXISTING ANCESTOR and verify it stays
    // under the canonical root — a symlinked parent that escapes is caught here.
    let mut ancestor = joined.as_path();
    let canonical_existing = loop {
        match ancestor.canonicalize() {
            Ok(p) => break p,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => match ancestor.parent() {
                Some(parent) => ancestor = parent,
                // We always reach `canonical_root` (it canonicalized above), so
                // a None here means we climbed past it — treat as outside.
                None => return Err(format!("path is outside the vault: {doc_path}")),
            },
            Err(e) => return Err(format!("could not resolve path: {e}")),
        }
    };
    if !canonical_existing.starts_with(&canonical_root) {
        return Err(format!("path is outside the vault: {doc_path}"));
    }

    Ok(())
}

/// `true` for directory names the vault walk must never descend into:
/// hidden dirs (`.obsidian`, `.git`, `.claude`, …) and vendored
/// `node_modules` trees (an attachment carrying a JS project would
/// otherwise flood the index with library READMEs).
fn is_skipped_dir(name: &str) -> bool {
    name.starts_with('.') || name == "node_modules"
}

/// `true` when `rel_path` (vault-relative, forward slashes) matches one of
/// the user's exclude `patterns`: entries containing `/` are path prefixes
/// (`attachments/demo`), bare names match any path component (`drafts`).
fn is_excluded(rel_path: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| {
        let p = p.trim_matches('/');
        if p.is_empty() {
            false
        } else if p.contains('/') {
            rel_path == p || rel_path.starts_with(&format!("{p}/"))
        } else {
            rel_path.split('/').any(|c| c == p)
        }
    })
}

/// Recursively collect every `*.md` file under `root` (hand-rolled
/// stack-based walk — no new crate dep). Hidden dirs and `node_modules`
/// are always skipped ([`is_skipped_dir`]); `exclude` adds the vault's
/// configured patterns ([`is_excluded`]).
fn walk_markdown_files(root: &Path, exclude: &[String]) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue, // vault_root itself missing, or unreadable subdir: skip
        };
        for entry in entries {
            let entry = entry.with_context(|| format!("reading dir entry in {}", dir.display()))?;
            let path = entry.path();
            if path.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let dir_rel = vault_relative_path(root, &path).unwrap_or_default();
                if !is_skipped_dir(&name) && !is_excluded(&dir_rel, exclude) {
                    stack.push(path);
                }
            } else if path.extension().is_some_and(|ext| ext == "md") {
                let rel = vault_relative_path(root, &path).unwrap_or_default();
                if !is_excluded(&rel, exclude) {
                    out.push(path);
                }
            }
        }
    }
    Ok(out)
}

/// Keep only the coherent TOP CLUSTER of vector hits: those within `window`
/// cosine of the best score. Recall-first replacement for the absolute
/// `drop_below_floor` — it NEVER empties a non-empty result set (the old
/// per-model floor silently zeroed genuine e5 matches that cluster at
/// ~0.83–0.87, just under a 0.85 floor). Relative to each query's own top, so
/// it is model-agnostic. Input is assumed sorted by score descending (as
/// `VectorStore::search` returns); the max is taken defensively regardless.
fn keep_top_cluster(hits: Vec<(String, f32)>, window: f32) -> Vec<(String, f32)> {
    let Some(top) = hits
        .iter()
        .map(|(_, s)| *s)
        .fold(None, |acc: Option<f32>, s| {
            Some(acc.map_or(s, |a| a.max(s)))
        })
    else {
        return hits; // empty in → empty out
    };
    let cutoff = top - window;
    hits.into_iter().filter(|(_, s)| *s >= cutoff).collect()
}

/// A single fused, resolved search hit.
pub struct Hit {
    pub chunk_id: String,
    pub doc_path: String,
    pub heading_path: String,
    pub score: f64,
    pub snippet: String,
    /// Calibrated 0–1 cross-encoder relevance, present when the Tier-2
    /// rerank stage scored this hit. `None` = unreranked (stage skipped or
    /// failed for the whole query — the reranked pool is
    /// `max(min_candidates, top_k)`, so every returned hit is normally
    /// reranked; nothing beyond that pool survives truncation to `top_k`).
    pub rerank_score: Option<f32>,
}

/// Per-chunk metadata stored in `engine.redb` (the text and heading path
/// that `lex`/`vector` don't retain).
#[derive(Serialize, Deserialize)]
struct ChunkMeta {
    doc_path: String,
    heading_path: String,
    chunk_index: usize,
    text: String,
}

/// How an [`Engine`] obtains its embedder. Production opens via
/// [`Engine::open`] and gets [`EmbedSource::Lazy`], which defers the
/// (multi-GB, network) `fastembed` model download until the first
/// `index_doc`/`query`/`vector_search`/`rebuild` that actually needs it —
/// so lex-only search, `status`, and `get` never pay for a download. Tests
/// open via [`Engine::open_with_embedder`] and get [`EmbedSource::Injected`],
/// a pre-built (typically fake) embedder with no download at all.
enum EmbedSource {
    /// Real embedder, constructed lazily from `model_name` + `cache_dir` on
    /// first use (see [`Engine::embedder`]).
    Lazy(OnceCell<Box<dyn Embed>>),
    /// Pre-built embedder, used directly with no lazy construction. Only
    /// constructed by the test-only [`Engine::open_with_embedder`] /
    /// [`Engine::rebuild_with_embedder`] seams, so it reads as dead code in a
    /// non-test build even though `embedder()`/`rebuild_inner` handle it.
    #[cfg_attr(not(test), allow(dead_code))]
    Injected(Box<dyn Embed>),
}

/// How an [`Engine`] obtains its reranker (the Tier-2 cross-encoder stage).
/// Mirrors [`EmbedSource`] with one extra honesty rule: the lazy path
/// resolves to `None` — skip reranking, never fail the query — when the
/// model is not downloaded, the build has no `semantic` feature, or
/// construction fails. Downloads belong to `reindex`, not the query path.
enum RerankSource {
    /// Real reranker, constructed lazily on the first reranked query.
    /// `None` inside the cell means resolution ran and reranking is skipped.
    Lazy(OnceCell<Option<Box<dyn Rerank>>>),
    /// Pre-built reranker injected by the test-only
    /// [`Engine::set_reranker_for_tests`] seam.
    #[cfg_attr(not(test), allow(dead_code))]
    Injected(Box<dyn Rerank>),
}

/// Engine-facing rerank settings, mapped from the config's `search.reranker`
/// block by the CLI layer (this crate does not read config files).
#[derive(Debug, Clone, PartialEq)]
pub struct RerankSettings {
    /// Master switch. Reranking is default-on: an absent config block means
    /// exactly these defaults.
    pub enabled: bool,
    /// Reranker registry name (see [`rerank::reranker_registry`]).
    pub model: String,
    /// Minimum fused-candidate pool fed to the cross-encoder — a FLOOR, not a
    /// ceiling: [`Engine::apply_rerank`] actually reranks
    /// `max(min_candidates, top_k)`, auto-raised so every result the caller
    /// returns is always reranked. `min_candidates` only matters when it
    /// exceeds `top_k` (a wider pool than the return size improves quality).
    pub min_candidates: usize,
    /// Gate: reranked hits scoring below this are dropped, subject to the
    /// never-empty floor ([`RERANK_NO_MATCH_KEEP`]).
    pub min_score: f32,
}

impl Default for RerankSettings {
    /// Defensive note: `enabled: true` here means a freshly-`Engine::open`ed
    /// engine (before any `set_rerank_settings` call) reports reranking as
    /// ON. Every production `open` path applies `set_rerank_settings` from
    /// the vault's config immediately after opening (see
    /// `search_common::open_engine`, `server::internal::try_open_held_engine`),
    /// so this default is never actually observed there. But a future call
    /// site that reads [`Engine::rerank_enabled`] before installing settings
    /// would silently get this default-on value instead of the vault's real
    /// `search.reranker.enabled` — install settings first.
    fn default() -> Self {
        Self {
            enabled: true,
            model: rerank::reranker_registry()[0].name.to_string(),
            // 10, not 30: calibration on real ob-1 (2026-07-06) showed every
            // golden-set match already lands in the top ~5 after rerank, while
            // bge-reranker-v2-m3 costs ~70 ms/candidate on CPU — 30 made a warm
            // search ~2 s (worse on a Pi). 10 keeps the quality and cuts rerank
            // compute ~3×. Keep in sync with `RerankerConfig`'s config default.
            min_candidates: 10,
            min_score: DEFAULT_RERANK_MIN_SCORE,
        }
    }
}

/// The assembled search engine: lexical index, vector store, embedder, and
/// a `redb` metadata database, all rooted at one `cache_dir`.
///
/// The embedder is obtained via [`EmbedSource`]: production ([`Engine::open`])
/// constructs the real `fastembed` embedder lazily on first use — `fastembed`
/// downloads the model file on first construction, so eager construction
/// would make every command (including lex-only search, `status`, and `get`)
/// pay for a model download. Only `index_doc`, `query`, `vector_search`, and
/// `rebuild` actually need the embedder. Tests inject a fake via
/// [`Engine::open_with_embedder`].
pub struct Engine {
    lex: LexIndex,
    vec: VectorStore,
    /// Vault-configured index-exclusion patterns (`search.exclude`), applied
    /// on top of the built-in skips by every vault walk (`reindex_all`,
    /// `status`). Empty by default; set via [`Engine::set_exclude_patterns`].
    exclude_patterns: Vec<String>,
    model_name: String,
    /// Split-layout resolver for this collection's cache root. All on-disk
    /// paths (index artifacts + the hf-hub model cache base) go through it,
    /// so every consumer sees the same `models/` + `index/` layout the
    /// eager `migrate()` on open established.
    layout: CollectionLayout,
    embedder: EmbedSource,
    /// Tier-2 rerank stage configuration; see [`Engine::set_rerank_settings`].
    rerank_settings: RerankSettings,
    /// Tier-2 reranker source; see [`RerankSource`].
    reranker: RerankSource,
    /// True once a rerank RUNTIME failure has been logged for this engine
    /// instance. Unlike the load failure (OnceCell-cached, logs once by
    /// construction), the runtime path re-runs every query — without this
    /// flag a persistently failing model would spam a long-running daemon's
    /// stderr on every search.
    rerank_error_logged: std::cell::Cell<bool>,
    /// Same rate-limit for corrupt chunk-meta warnings during rerank passage
    /// lookup ([`Engine::chunk_texts`]).
    chunk_corruption_logged: std::cell::Cell<bool>,
    /// Test seam: when true, [`Engine::fetch_reranker_model`] skips the real
    /// hf-hub download. A unit test that only needs a reindex to EMIT the
    /// `LoadingReranker` event must not pull the ~570 MB model over the
    /// network. Always false in production; set via
    /// [`Engine::skip_reranker_fetch_for_tests`].
    skip_reranker_fetch: bool,
    meta: Database,
    /// Held for the engine's whole lifetime purely for its exclusive-open
    /// side effect (never read/written to) — see
    /// [`CollectionLayout::lock_path`] (#223). Reuses redb's own
    /// crash-safe, process-exclusive open (auto-released by the OS if this
    /// process dies) rather than adding a new file-locking dependency.
    _collection_lock: Database,
}

/// Pure decision for the reindex-time reranker fetch: fetch only when
/// reranking is enabled AND the configured model isn't already downloaded.
/// An unknown model name is treated as "downloaded" by the caller (mirrors
/// [`Engine::build_reranker`]'s skip-on-unknown-model behavior), so this
/// function never needs to know about the registry itself.
fn should_fetch_reranker(enabled: bool, downloaded: bool) -> bool {
    enabled && !downloaded
}

/// Truncate `text` to at most `max_chars` characters on a char boundary,
/// appending "…" if truncation occurred. Safe for multibyte (e.g. Thai)
/// text since it operates on `char`s, never raw byte offsets.
fn truncate_snippet(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push('…');
    }
    out
}

impl Engine {
    /// Open/create an engine rooted at `cache_dir`. `embed_model` selects
    /// the embedding model (dims resolved via [`embed::model_dims`]). The
    /// real embedder is constructed lazily on first use — `open` itself never
    /// downloads a model.
    pub fn open(cache_dir: &Path, embed_model: &str) -> Result<Self> {
        let dims = embed::model_dims(embed_model);
        Self::open_inner(
            cache_dir,
            embed_model,
            dims,
            EmbedSource::Lazy(OnceCell::new()),
        )
        // Classify redb's single-process lock ("database already open") into
        // the typed `EngineBusy` error so every CLI surface can report honest
        // contention instead of a generic failure (v3.4.6). The lock can be
        // tripped by either the vector-meta db or `engine.redb`, so classify
        // once here at the shared open boundary.
        .map_err(crate::error::classify_open_error)
    }

    /// Open/create an engine rooted at `cache_dir` with a caller-supplied
    /// embedder, used directly (no lazy download). The vector store is opened
    /// at `embedder.dims()`, and the recorded active-model name is
    /// `embed_model`.
    ///
    /// This is the crate-visible test seam: it lets tests exercise the full
    /// index/query/rebuild logic against a deterministic in-memory embedder
    /// without pulling a multi-GB model over the network. Production always
    /// uses [`Engine::open`] (lazy real embedder).
    #[cfg(test)]
    pub(crate) fn open_with_embedder(
        cache_dir: &Path,
        embed_model: &str,
        embedder: Box<dyn Embed>,
    ) -> Result<Self> {
        let dims = embedder.dims();
        Self::open_inner(
            cache_dir,
            embed_model,
            dims,
            EmbedSource::Injected(embedder),
        )
        .map_err(crate::error::classify_open_error)
    }

    /// Shared open/create path for both [`Engine::open`] and
    /// [`Engine::open_with_embedder`]: create the cache dir, open the lex
    /// index, open the vector store at `dims`, and open/seed the redb
    /// metadata database (recording `embed_model` as the active model if
    /// none is recorded yet).
    fn open_inner(
        cache_dir: &Path,
        embed_model: &str,
        dims: usize,
        embedder: EmbedSource,
    ) -> Result<Self> {
        std::fs::create_dir_all(cache_dir)
            .with_context(|| format!("creating cache dir {}", cache_dir.display()))?;

        let layout = CollectionLayout::new(cache_dir);

        // Collection-level advisory lock (#223), acquired BEFORE `migrate()`
        // and every index artifact — because `migrate()` itself (the
        // `fs::rename` of legacy `tantivy`/`vectors`/`engine.redb`/`models--*`
        // from the legacy root into the split layout) is exactly the
        // concurrent-unsafe I/O this lock exists to serialize. Two processes
        // racing to open the same still-legacy collection (e.g. a long-lived
        // `onebrain mcp` daemon + a CLI command right after an upgrade) must
        // NOT both enter `migrate()`: one's rename would then hit ENOENT
        // after the other moved the source, and that raw I/O error is NOT
        // recognized by `classify_open_error`, so the caller would get a
        // confusing failure instead of the honest `EngineBusy`. Taking the
        // lock first serializes migration and also spares a busy collection a
        // wasted, unprotected migrate attempt.
        //
        // The lock path is fixed at the collection root regardless of
        // legacy/split state — see `CollectionLayout::lock_path` — so two
        // openers always contend here even if they'd otherwise resolve to two
        // different physical `engine.redb` files (redb's own per-file lock
        // can't see that collision). It reuses redb's own crash-safe,
        // process-exclusive open; a lost race classifies as `EngineBusy`.
        let lock_path = layout.lock_path();
        let collection_lock = Database::create(&lock_path)
            .with_context(|| format!("acquiring collection lock at {}", lock_path.display()))?;

        // Eager migration on the write path, now under the collection lock:
        // fold any legacy flat artifacts (and `models--*` dirs) into the
        // `models/` + `index/` split, and (unconditionally) create both
        // subdirs so redb's `Database::create` — which does NOT create
        // parents — finds `index/` already present.
        layout
            .migrate()
            .with_context(|| format!("migrating cache layout at {}", cache_dir.display()))?;

        let vec = VectorStore::open(&layout.index_artifact("vectors"), dims)?;

        let meta_path = layout.index_artifact("engine.redb");
        let meta = Database::create(&meta_path)
            .with_context(|| format!("opening redb database {}", meta_path.display()))?;
        {
            let write_txn = meta.begin_write()?;
            write_txn.open_table(CHUNK_META)?;
            write_txn.open_table(DOC_CHUNKS)?;
            write_txn.open_table(DOC_HASHES)?;
            write_txn.open_table(LEX_HASHES)?;
            {
                let mut header = write_txn.open_table(ENGINE_HEADER)?;
                if header.get(ACTIVE_MODEL_KEY)?.is_none() {
                    header.insert(ACTIVE_MODEL_KEY, embed_model)?;
                }
                // Mint the index-instance nonce exactly once, when this
                // `engine.redb` is first created (or after a `--force`/wipe
                // dropped the header). Present ⟹ same index instance ⟹ keep it
                // stable across every reopen and reindex; absent ⟹ fresh index
                // ⟹ a new nonce so the memo cache can't reuse a prior instance's
                // key space (design §3a / MAJOR 2).
                if header.get(INDEX_NONCE_KEY)?.is_none() {
                    header.insert(INDEX_NONCE_KEY, fresh_index_nonce().as_str())?;
                }
            }
            write_txn.commit()?;
        }

        // A tantivy schema change (e.g. giving `heading_path` the script-aware
        // tokenizer so it can be queried) makes an older vault's index
        // un-openable. Rather than failing the whole engine on upgrade, wipe
        // and repopulate from redb — which holds every chunk's text.
        //
        // ORDERING (B1): this is deliberately the LAST thing `open` does, after
        // the vector store and redb have opened successfully. The wipe is the
        // only destructive step here, and everything before it can fail for
        // ordinary reasons (a locked/corrupt redb, a vectors dir we can't
        // read). Wiping first meant such a failure returned an `Err` from a
        // point where the lex index was already gone but nothing had recorded
        // that fact — a dead index with no crash to explain it. Doing it last
        // means a failure on either of those lines leaves the old lex index
        // fully intact. The marker below covers the window that ordering
        // cannot: an actual crash/Ctrl-C between the wipe and the commit.
        let tantivy_dir = layout.index_artifact("tantivy");
        let (lex, lex_was_reset) = LexIndex::open_or_reset(&tantivy_dir)?;
        // Crash-safe, and the authority: the marker outlives the process, the
        // in-memory flag does not. `||` rather than the marker alone so a
        // brand-new reset still repopulates even if the marker file vanished
        // underneath us (external cleanup, aggressive tmp reaper).
        let needs_repopulate = lex_was_reset || lex::rebuild_pending(&tantivy_dir);

        let mut engine = Engine {
            lex,
            vec,
            exclude_patterns: Vec::new(),
            model_name: embed_model.to_string(),
            layout,
            embedder,
            rerank_settings: RerankSettings::default(),
            reranker: RerankSource::Lazy(OnceCell::new()),
            rerank_error_logged: std::cell::Cell::new(false),
            chunk_corruption_logged: std::cell::Cell::new(false),
            skip_reranker_fetch: false,
            meta,
            _collection_lock: collection_lock,
        };
        if needs_repopulate {
            // C6: a silent multi-minute rebuild looks hung, and a user who
            // Ctrl-Cs a rebuild they think is stuck is exactly what creates
            // the B1 state. Announce it, then tick. stderr only, so JSON on
            // stdout stays clean. `Engine::open` has no progress callback to
            // plumb through, so this is the crate's own reporting; callers
            // that want structured progress call
            // [`Engine::repopulate_lex_from_meta_with_progress`] directly.
            engine.repopulate_lex_from_meta_with_progress(&mut |done, total| {
                if done == 0 {
                    eprintln!(
                        "onebrain-search: keyword index schema changed — rebuilding {total} \
                         chunk(s) from stored metadata (no files re-read, nothing re-embedded)"
                    );
                } else if done == total || done % REPOPULATE_PROGRESS_EVERY == 0 {
                    eprintln!("onebrain-search: rebuilding keyword index {done}/{total}");
                }
            })?;
        }
        Ok(engine)
    }

    /// Rebuild the lex index from stored chunk metadata: clear it, then re-add
    /// every stored chunk.
    ///
    /// **Idempotent by construction, on ANY starting state** — empty or fully
    /// populated. It used to assume it ran only after
    /// [`LexIndex::open_or_reset`] had wiped a stale-schema index, and that
    /// assumption was wrong the moment the rebuild became *marker*-driven:
    /// when only the marker triggers the rebuild the schema MATCHES, so
    /// `open_or_reset` wipes nothing and returns a fully populated index. With
    /// [`LexIndex::add`] never replacing, the rebuild was then appended on top
    /// of the existing documents and doubled the index — every repeat doubling
    /// again, silently, while `lex_health` still reported "healthy" (it only
    /// flagged an EMPTY index). Duplicate documents also corrupt BM25 document
    /// frequencies and average field length, so relevance decays with each
    /// repeat. Hence the unconditional [`LexIndex::clear`] first.
    ///
    /// `chunk_meta` already holds each chunk's `doc_path`, `heading_path`,
    /// `chunk_index` and `text`, so the rebuild touches no vault files and —
    /// crucially — never loads the embedding model: vectors and
    /// `doc_hashes`/`lex_hashes` are untouched and stay valid, since the
    /// content itself did not change.
    ///
    /// Returns the number of chunks restored. On return, the index holds
    /// exactly that many documents — no leftovers from before.
    ///
    /// The clear is unconditional, including when `chunk_meta` is EMPTY (the
    /// result is then an empty lex index). That is the honest reading of the
    /// contract "the lex index mirrors `chunk_meta`", and it destroys nothing
    /// usable: a lex document whose `chunk_id` has no `chunk_meta` row is
    /// skipped by [`Engine::resolve_hits`], so it can never surface as a hit —
    /// it only inflates BM25's corpus statistics.
    pub fn repopulate_lex_from_meta(&mut self) -> Result<usize> {
        self.repopulate_lex_from_meta_with_progress(&mut |_, _| {})
    }

    /// Like [`Engine::repopulate_lex_from_meta`] but reports progress as
    /// `(done, total)` chunk counts: once as `(0, total)` before the first
    /// chunk is added, then after every chunk. On a large vault this rebuild
    /// takes long enough to look hung, and a user who interrupts it is exactly
    /// what produces the half-migrated state the marker exists to repair — so
    /// silence here is a correctness hazard, not only a UX one.
    ///
    /// Skip-not-fail on corruption: a `chunk_meta` value that no longer
    /// deserializes is logged (rate-limited, same convention as
    /// [`Engine::chunk_texts`]) and skipped, never propagated. Propagating
    /// aborted `Engine::open` itself — *after* the wipe — so one bad record
    /// made the collection permanently unopenable; losing one chunk from the
    /// keyword index is strictly better, and `reindex --force` restores it.
    ///
    /// The rebuild marker is cleared only after [`LexIndex::commit`] returns,
    /// so an interruption anywhere before that leaves the marker in place and
    /// the next `Engine::open` simply tries again.
    ///
    /// **Why the clear + adds + single commit is crash-safe.**
    /// [`LexIndex::clear`] writes nothing to disk — it only empties tantivy's
    /// in-memory segment registers (see its docs) — so the ONE `commit` below
    /// is the only durable step, and it publishes exactly the documents added
    /// in this call. A crash before it leaves the previous index byte-for-byte
    /// intact (duplicated or not) *and* the marker still on disk, so the next
    /// `Engine::open` re-runs this whole function and clears again. There is
    /// no intermediate commit and therefore no window in which the index is
    /// observably empty or half-rebuilt.
    pub fn repopulate_lex_from_meta_with_progress(
        &mut self,
        progress: &mut dyn FnMut(usize, usize),
    ) -> Result<usize> {
        // FIRST, unconditionally: `add` appends, so anything already in the
        // index would otherwise survive alongside the freshly added copies.
        // Deliberately not conditional on "did open_or_reset wipe?" — the
        // marker-only rebuild path reaches here over a POPULATED index, and
        // this function is also called straight from `doctor --fix`.
        self.lex.clear()?;
        let read_txn = self.meta.begin_read()?;
        let chunk_meta = read_txn.open_table(CHUNK_META)?;
        let total = chunk_meta.len()? as usize;
        progress(0, total);
        let mut restored = 0usize;
        let mut skipped = 0usize;
        for entry in chunk_meta.iter()? {
            let (key, value) = entry?;
            let chunk_id = key.value().to_string();
            let record: ChunkMeta = match serde_json::from_str(value.value()) {
                Ok(record) => record,
                Err(e) => {
                    skipped += 1;
                    if !self.chunk_corruption_logged.get() {
                        self.chunk_corruption_logged.set(true);
                        eprintln!(
                            "onebrain-search: chunk meta for {chunk_id} is corrupt ({e}) — \
                             skipped while rebuilding the keyword index; a `reindex --force` \
                             rebuilds it (further corrupt-chunk warnings on this engine will \
                             not be logged)"
                        );
                    }
                    progress(restored + skipped, total);
                    continue;
                }
            };
            self.lex.add(&Chunk {
                chunk_id,
                doc_path: record.doc_path,
                heading_path: record.heading_path,
                chunk_index: record.chunk_index,
                text: record.text,
            })?;
            restored += 1;
            progress(restored + skipped, total);
        }
        drop(chunk_meta);
        drop(read_txn);
        self.lex.commit()?;
        // The rebuild SUCCEEDED — the commit above is the durable part. A
        // failure to remove the bookkeeping marker afterwards must not turn
        // that success into an `Err`: `Engine::open` propagates this, and the
        // next open would see matching schema + pending marker → same rebuild
        // → same failure, making the collection permanently unopenable on a
        // read-only `index/`. Warn loudly (never silently swallow) and carry
        // on; the only cost of a lingering marker is a redundant rebuild.
        if let Err(e) = lex::clear_rebuild_marker(&self.layout.index_artifact("tantivy")) {
            eprintln!(
                "onebrain-search: keyword index rebuilt successfully ({restored} chunk(s)), but \
                 the rebuild marker could not be cleared ({e:#}) — the next open will redo this \
                 rebuild. Fix the permissions on the collection's index dir, or remove the \
                 marker file by hand."
            );
        }
        Ok(restored)
    }

    /// Cheap consistency probe over the keyword index, for `doctor` / `status`.
    ///
    /// Two counts and a flag, no query and no full scan: tantivy's committed
    /// doc count, redb's `chunk_meta` row count, and whether a lex rebuild is
    /// still marked pending. See [`LexHealth::is_dead`] for the state this
    /// exists to catch.
    pub fn lex_health(&self) -> Result<LexHealth> {
        let read_txn = self.meta.begin_read()?;
        let chunk_meta = read_txn.open_table(CHUNK_META)?;
        Ok(LexHealth {
            lex_docs: self.lex.num_docs()?,
            chunk_meta: chunk_meta.len()?,
            rebuild_pending: lex::rebuild_pending(&self.layout.index_artifact("tantivy")),
        })
    }

    /// Path to the vector store directory, resolved through the split layout
    /// (`index/vectors` post-migration, legacy root fallback otherwise).
    fn vectors_dir(&self) -> PathBuf {
        self.layout.index_artifact("vectors")
    }

    /// The active embedding model recorded in `engine_header`, if any has
    /// been recorded yet.
    fn stored_active_model(&self) -> Result<Option<String>> {
        let read_txn = self.meta.begin_read()?;
        let header = read_txn.open_table(ENGINE_HEADER)?;
        Ok(header.get(ACTIVE_MODEL_KEY)?.map(|v| v.value().to_string()))
    }

    /// True if the engine's stored active-model matches `cfg_model` (i.e. no
    /// rebuild needed). When no model has been recorded yet (fresh index —
    /// shouldn't normally happen since `open` always records one), treat the
    /// current `self.model_name` as active.
    pub fn active_model_matches(&self, cfg_model: &str) -> Result<bool> {
        let active = self
            .stored_active_model()?
            .unwrap_or_else(|| self.model_name.clone());
        Ok(active == cfg_model)
    }

    /// Switch the embedding model: record the new active model, wipe ONLY
    /// the vector store (dropping all vectors), and re-embed every existing
    /// chunk from `chunk_meta` with the new model. The lex/BM25 index and
    /// `chunk_meta`/`doc_chunks` are model-independent and are NOT touched.
    /// Returns how many chunks were re-embedded.
    ///
    /// The new vector store is opened at the new model's dims (resolved via
    /// [`embed::model_dims`]) and the embedder is reset to a fresh lazy one,
    /// so the real model download happens on the first re-embed here — the
    /// production behavior is unchanged from before the [`Embed`]-trait
    /// refactor.
    pub fn rebuild(&mut self, new_model: &str) -> Result<usize> {
        self.rebuild_with_progress(new_model, &mut |_, _| {})
    }

    /// Like [`Engine::rebuild`] but reports live `(re-embedded, total)` chunk
    /// counts: once as `(0, total)` right before the first embed call (i.e.
    /// before the potentially slow model download / load), then after every
    /// internal batch. Never fires for an empty index (nothing to re-embed).
    pub fn rebuild_with_progress(
        &mut self,
        new_model: &str,
        progress: &mut dyn FnMut(usize, usize),
    ) -> Result<usize> {
        let new_dims = embed::model_dims(new_model);
        self.rebuild_inner(
            new_model,
            new_dims,
            EmbedSource::Lazy(OnceCell::new()),
            progress,
        )
    }

    /// Like [`Engine::rebuild`] but with a caller-supplied embedder used
    /// directly at its own `dims()` (no lazy download). Crate-visible test
    /// seam mirroring [`Engine::open_with_embedder`].
    #[cfg(test)]
    pub(crate) fn rebuild_with_embedder(
        &mut self,
        new_model: &str,
        embedder: Box<dyn Embed>,
        progress: &mut dyn FnMut(usize, usize),
    ) -> Result<usize> {
        let new_dims = embedder.dims();
        self.rebuild_inner(
            new_model,
            new_dims,
            EmbedSource::Injected(embedder),
            progress,
        )
    }

    /// Shared rebuild path: collect the re-embed worklist, wipe the vector
    /// store, swap in `new_source` + `new_dims`, re-embed, and record
    /// `new_model` as active. `new_dims` must match `new_source`'s embedding
    /// width.
    fn rebuild_inner(
        &mut self,
        new_model: &str,
        new_dims: usize,
        new_source: EmbedSource,
        progress: &mut dyn FnMut(usize, usize),
    ) -> Result<usize> {
        // 1. Collect every chunk (id, text) from chunk_meta up front, before
        // wiping anything, so we have the full re-embed worklist.
        let chunks: Vec<(String, String)> = {
            let read_txn = self.meta.begin_read()?;
            let chunk_meta = read_txn.open_table(CHUNK_META)?;
            let mut out = Vec::new();
            for entry in chunk_meta.iter()? {
                let (chunk_id_guard, encoded_guard) = entry?;
                let chunk_id = chunk_id_guard.value().to_string();
                let record: ChunkMeta = serde_json::from_str(encoded_guard.value())?;
                out.push((chunk_id, record.text));
            }
            out
        };

        // 2. Delete the old vector store directory so it can be recreated
        // fresh at the new model's dims (VectorStore::open errors on a dims
        // mismatch against what's on disk).
        let vectors_dir = self.vectors_dir();
        match std::fs::remove_dir_all(&vectors_dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("removing vector store dir {}", vectors_dir.display())
                })
            }
        }

        // 3. Swap in the new model: update model_name, install the new
        // embedder source (a fresh lazy embedder in production, or an injected
        // one in tests), and reopen the vector store at the new dims.
        self.model_name = new_model.to_string();
        self.embedder = new_source;
        self.vec = VectorStore::open(&vectors_dir, new_dims)?;

        // 4. Re-embed in batches so progress can be reported between calls
        // (fastembed also batches internally; the outer batch just bounds
        // how long the UI goes without an update). Skipped entirely when
        // there are no chunks, so an empty index never constructs the
        // embedder (no model download).
        if !chunks.is_empty() {
            const REBUILD_EMBED_BATCH: usize = 64;
            let total = chunks.len();
            // (0, total) before the first embed call — the model download /
            // load stall happens inside it, so the UI can show the total
            // while that runs.
            progress(0, total);
            let mut done = 0usize;
            for batch in chunks.chunks(REBUILD_EMBED_BATCH) {
                let texts: Vec<String> = batch.iter().map(|(_, text)| text.clone()).collect();
                let vectors = self.embedder()?.embed_passages(&texts)?;
                for ((chunk_id, _text), vector) in batch.iter().zip(vectors.iter()) {
                    self.vec.add(chunk_id, vector)?;
                }
                done += batch.len();
                progress(done, total);
            }
        }

        // 5. Record the new active model.
        let write_txn = self.meta.begin_write()?;
        {
            let mut header = write_txn.open_table(ENGINE_HEADER)?;
            header.insert(ACTIVE_MODEL_KEY, new_model)?;
        }
        write_txn.commit()?;

        Ok(chunks.len())
    }

    /// Install the vault's `search.exclude` patterns — applied by every
    /// vault walk on top of the built-in skips (hidden dirs, node_modules).
    pub fn set_exclude_patterns(&mut self, patterns: Vec<String>) {
        self.exclude_patterns = patterns;
    }

    /// Return the embedder, constructing the real one lazily on first use
    /// for [`EmbedSource::Lazy`] (production) or handing back the injected
    /// one for [`EmbedSource::Injected`] (tests).
    ///
    /// For the lazy case this is the ONLY place `embed::new` is called — the
    /// first call to `index_doc`/`query`/`vector_search`/`rebuild` is when a
    /// model download actually happens, not `Engine::open`.
    ///
    /// `std::cell::OnceCell::get_or_try_init` is nightly-only
    /// (`once_cell_try`, #109737), so init is spelled out manually: check
    /// `get()`, and on miss build the embedder and `set()` it (the `Ok(())`
    /// from `set` is discarded — another thread can't have raced us since
    /// `Engine` isn't `Sync`/shared across threads here).
    fn embedder(&self) -> Result<&dyn Embed> {
        match &self.embedder {
            EmbedSource::Injected(e) => Ok(e.as_ref()),
            EmbedSource::Lazy(cell) => {
                if let Some(e) = cell.get() {
                    return Ok(e.as_ref());
                }
                #[cfg(feature = "semantic")]
                {
                    let e: Box<dyn Embed> =
                        Box::new(embed::new(&self.model_name, &self.layout.models_base())?);
                    let _ = cell.set(e);
                    Ok(cell.get().expect("embedder was just set above").as_ref())
                }
                #[cfg(not(feature = "semantic"))]
                {
                    // Silence unused-field warnings in the lex-only build; the
                    // real embedder is what would consume these.
                    let _ = &self.model_name;
                    let _ = &self.layout;
                    anyhow::bail!(SEMANTIC_UNAVAILABLE)
                }
            }
        }
    }

    /// The Tier-2 reranker, or `None` when reranking is skipped (disabled,
    /// model not downloaded, lex-only build, or load failure). Lazy like
    /// [`Engine::embedder`], but skip-not-fail: a query must never error
    /// because reranking is unavailable.
    fn reranker(&self) -> Option<&dyn Rerank> {
        if !self.rerank_settings.enabled {
            return None;
        }
        match &self.reranker {
            RerankSource::Injected(r) => Some(r.as_ref()),
            RerankSource::Lazy(cell) => cell.get_or_init(|| self.build_reranker()).as_deref(),
        }
    }

    /// Construct the real reranker for the lazy path. `None` = skip, with a
    /// stderr note only for the genuinely unexpected load-failure case (a
    /// missing model is normal until the first `reindex` fetches it).
    fn build_reranker(&self) -> Option<Box<dyn Rerank>> {
        #[cfg(feature = "semantic")]
        {
            let info = rerank::reranker_registry()
                .iter()
                .find(|m| m.name == self.rerank_settings.model)?;
            if !rerank::reranker_download_status(info, self.layout.root()).downloaded {
                return None;
            }
            match rerank::new(&self.rerank_settings.model, &self.layout.models_base()) {
                Ok(r) => Some(Box::new(r) as Box<dyn Rerank>),
                Err(e) => {
                    eprintln!(
                        "onebrain-search: reranker '{}' failed to load — results are unreranked: {e:#}",
                        self.rerank_settings.model
                    );
                    None
                }
            }
        }
        #[cfg(not(feature = "semantic"))]
        {
            None
        }
    }

    /// Download the configured reranker model if it isn't on disk yet — the
    /// reindex fetch path. This is DISTINCT from [`Engine::reranker`]: the
    /// lazy accessor deliberately skips (returns `None`) when the model is
    /// absent, because a query must never trigger a multi-hundred-MB
    /// download on the request path. Reindex, by contrast, WANTS the
    /// download, so it goes straight to [`rerank::new`] (which fetches via
    /// hf-hub + verifies the pinned sha256). Without this split the two
    /// contracts deadlock — reindex asks `reranker()` to download, but
    /// `reranker()` refuses to construct until already downloaded, so the
    /// model can never arrive (found on real ob-1, 2026-07-06).
    ///
    /// Skip-not-fail: an unknown model name or a download/verify error is
    /// logged and swallowed — a failed fetch leaves search unreranked, never
    /// fails the reindex.
    fn fetch_reranker_model(&self) {
        // Test seam: a unit test that only asserts the `LoadingReranker`
        // progress event (emitted by the caller BEFORE this call) must not
        // trigger the real ~570 MB hf-hub download. Only meaningful in the
        // semantic build (lex-only never downloads); the `not(semantic)` read
        // keeps the field live there without a needless early `return`.
        #[cfg(not(feature = "semantic"))]
        let _ = self.skip_reranker_fetch;
        #[cfg(feature = "semantic")]
        {
            if self.skip_reranker_fetch {
                return;
            }
            if !rerank::is_supported_reranker(&self.rerank_settings.model) {
                // A typo'd `search.reranker.model` is the one skip-not-fail
                // branch that would otherwise leave no trace in reindex stderr
                // (download/verify/load errors all log below) — say it once so
                // a config typo is diagnosable without cross-referencing status.
                eprintln!(
                    "onebrain-search: reranker model '{}' is not a known model — search stays unreranked (check `search.reranker.model`)",
                    self.rerank_settings.model
                );
                return;
            }
            if let Err(e) = rerank::new(&self.rerank_settings.model, &self.layout.models_base()) {
                eprintln!(
                    "onebrain-search: reranker model '{}' failed to fetch — search stays unreranked: {e:#}",
                    self.rerank_settings.model
                );
            }
        }
    }

    /// Install rerank settings (the CLI layer maps `search.reranker` here).
    /// Resets the lazy reranker source so a model change re-resolves.
    pub fn set_rerank_settings(&mut self, settings: RerankSettings) {
        self.rerank_settings = settings;
        self.reranker = RerankSource::Lazy(OnceCell::new());
        // A settings change (e.g. model swap) is a distinct event — re-arm
        // the once-per-engine failure log so a NEW model's failure is never
        // hidden by an old model's suppressed warning.
        self.rerank_error_logged.set(false);
    }

    /// Override just the `min_candidates` knob of the currently installed
    /// [`RerankSettings`] — e.g. a per-query `--min-candidates` CLI flag or
    /// API param overriding the vault's configured
    /// `search.reranker.min_candidates`. Unlike [`Engine::set_rerank_settings`],
    /// this does NOT reset the lazy reranker source: `min_candidates` only
    /// affects how wide a pool [`Engine::apply_rerank`] reranks, not which
    /// model loads, so there is nothing to re-resolve. Call AFTER the
    /// settings this overrides are installed (i.e. after `open_engine`'s
    /// `set_rerank_settings`).
    pub fn set_rerank_min_candidates(&mut self, min_candidates: usize) {
        self.rerank_settings.min_candidates = min_candidates;
    }

    /// Per-query override of the vault's configured `search.reranker.min_score`
    /// gate. Used to unify the CLI `--min-score` flag with the rerank gate:
    /// when reranking is active, `--min-score` filters by the calibrated 0–1
    /// `rerank_score` (this gate) instead of the raw retrieval score. Like
    /// [`Engine::set_rerank_min_candidates`], it does NOT reset the lazy
    /// reranker source — it only changes the gate threshold. Call AFTER
    /// `set_rerank_settings`.
    pub fn set_rerank_min_score(&mut self, min_score: f32) {
        self.rerank_settings.min_score = min_score;
    }

    /// Whether the Tier-2 reranker is turned on per the currently installed
    /// [`RerankSettings`] (`search.reranker.enabled` in `onebrain.yml`). Lets
    /// callers distinguish an explicit `enabled: false` (no rerank attempted,
    /// no hint should be shown) from "enabled but skipped" (model not
    /// downloaded / load failure — the unreranked hint IS warranted).
    ///
    /// Reads whatever [`RerankSettings`] is CURRENTLY installed — if this is
    /// called before [`Engine::set_rerank_settings`] on a freshly-opened
    /// engine, it reports `RerankSettings::default()`'s `enabled: true`
    /// rather than the vault's configured value (see the defensive note on
    /// that `Default` impl). Every production caller installs settings
    /// immediately after `open`, so this is a latent footgun, not a live bug.
    pub fn rerank_enabled(&self) -> bool {
        self.rerank_settings.enabled
    }

    /// Test seam mirroring [`Engine::open_with_embedder`]: inject a
    /// deterministic reranker. Call AFTER [`Engine::set_rerank_settings`] —
    /// installing settings resets the reranker source.
    #[cfg(test)]
    pub(crate) fn set_reranker_for_tests(&mut self, reranker: Box<dyn Rerank>) {
        self.reranker = RerankSource::Injected(reranker);
    }

    /// Test seam: suppress the reindex-time reranker download so a hermetic
    /// unit test can exercise the `LoadingReranker` progress event (emitted
    /// before the fetch) without pulling the real ~570 MB model over the
    /// network. Call before `reindex_all_with_progress`.
    #[cfg(test)]
    pub(crate) fn skip_reranker_fetch_for_tests(&mut self) {
        self.skip_reranker_fetch = true;
    }

    /// True when queries will actually be reranked: settings enabled AND a
    /// reranker is loaded (or loadable). Status/doctor surfaces use this for
    /// honest reporting.
    pub fn rerank_active(&self) -> bool {
        self.reranker().is_some()
    }

    /// Whether this engine can produce embeddings at all. Always `true` for an
    /// injected embedder (tests). For the lazy production source it's `true`
    /// only in a `semantic` build — a lex-only build has no ONNX runtime to
    /// construct the real embedder, so callers skip the vector side entirely.
    fn embedder_available(&self) -> bool {
        match &self.embedder {
            EmbedSource::Injected(_) => true,
            EmbedSource::Lazy(_) => cfg!(feature = "semantic"),
        }
    }

    /// Embed passage texts if an embedder is available, else `None` (lex-only
    /// build with no injected embedder). `Some(vec![])` for an empty input so
    /// index-alignment with `chunks` still holds. Used by [`Engine::index_doc`]
    /// so a lex-only build indexes the doc without populating the vector store.
    fn embed_passages_if_available(&self, texts: &[String]) -> Result<Option<Vec<Vec<f32>>>> {
        if !self.embedder_available() {
            return Ok(None);
        }
        if texts.is_empty() {
            return Ok(Some(Vec::new()));
        }
        Ok(Some(self.embedder()?.embed_passages(texts)?))
    }

    /// Chunk `content`, index into lex + embed + vector, and record chunk
    /// meta. Returns the number of chunks indexed.
    pub fn index_doc(&mut self, doc_path: &str, content: &str) -> Result<usize> {
        self.index_doc_mode(doc_path, content, IndexMode::Full)
    }

    /// Shared body of [`Engine::index_doc`]: chunk `content` and index into
    /// lex + chunk meta always; embed + vector store ONLY in
    /// [`IndexMode::Full`]. `LexOnly` never calls
    /// [`Engine::embed_passages_if_available`] — not even to get `None` back
    /// — so it never constructs the embedder, lazy or otherwise (see the
    /// `PanicEmbed` test double in `mod tests`, which panics on ANY embedder
    /// method).
    fn index_doc_mode(&mut self, doc_path: &str, content: &str, mode: IndexMode) -> Result<usize> {
        let chunks = chunk_markdown(doc_path, content, CHUNK_MAX_TOKENS, CHUNK_OVERLAP_TOKENS);

        // Batch-embed ALL chunk texts in one call — `Embedder::embed` takes a
        // slice and batches internally, so one call per doc is far cheaper
        // than one call per chunk. The returned vectors are index-aligned with
        // `chunks`.
        //
        // `embed_passages_if_available` yields `None` in a lex-only build (no
        // `semantic` feature and no injected embedder): the doc is then still
        // fully lex-indexed and its meta/hash recorded — only the vector store
        // is left unpopulated, so keyword search + the whole index lifecycle
        // work unchanged. Tests inject a fake embedder, so they still get
        // vectors regardless of the `semantic` feature.
        //
        // `IndexMode::LexOnly` skips this call entirely (rather than calling
        // it and discarding `Some(vectors)`): the whole point of a lex-only
        // reindex is to guarantee zero embedder interaction.
        let vectors = match mode {
            IndexMode::Full => {
                let texts: Vec<String> = chunks.iter().map(|c| c.text.clone()).collect();
                self.embed_passages_if_available(&texts)?
            }
            IndexMode::LexOnly => None,
        };

        let mut chunk_ids: Vec<String> = Vec::with_capacity(chunks.len());
        let write_txn = self.meta.begin_write()?;
        {
            let mut chunk_meta = write_txn.open_table(CHUNK_META)?;
            for (i, chunk) in chunks.iter().enumerate() {
                self.lex.add(chunk)?;

                if let Some(vectors) = &vectors {
                    self.vec.add(&chunk.chunk_id, &vectors[i])?;
                }

                let record = ChunkMeta {
                    doc_path: chunk.doc_path.clone(),
                    heading_path: chunk.heading_path.clone(),
                    chunk_index: chunk.chunk_index,
                    text: chunk.text.clone(),
                };
                let encoded = serde_json::to_string(&record)?;
                chunk_meta.insert(chunk.chunk_id.as_str(), encoded.as_str())?;
                chunk_ids.push(chunk.chunk_id.clone());
            }
        }
        {
            let mut doc_chunks = write_txn.open_table(DOC_CHUNKS)?;
            let encoded = serde_json::to_string(&chunk_ids)?;
            doc_chunks.insert(doc_path, encoded.as_str())?;
        }
        write_txn.commit()?;

        self.lex.commit()?;

        Ok(chunks.len())
    }

    /// Remove all chunks of `doc_path` from lex + vector + meta.
    pub fn remove_doc(&mut self, doc_path: &str) -> Result<()> {
        let chunk_ids: Vec<String> = {
            let read_txn = self.meta.begin_read()?;
            let doc_chunks = read_txn.open_table(DOC_CHUNKS)?;
            match doc_chunks.get(doc_path)? {
                Some(v) => serde_json::from_str(v.value())?,
                None => Vec::new(),
            }
        };

        let write_txn = self.meta.begin_write()?;
        {
            let mut chunk_meta = write_txn.open_table(CHUNK_META)?;
            for chunk_id in &chunk_ids {
                self.lex.delete(chunk_id)?;
                self.vec.remove(chunk_id)?;
                chunk_meta.remove(chunk_id.as_str())?;
            }
        }
        {
            let mut doc_chunks = write_txn.open_table(DOC_CHUNKS)?;
            doc_chunks.remove(doc_path)?;
        }
        write_txn.commit()?;

        self.lex.commit()?;

        Ok(())
    }

    /// Resolve a list of `(chunk_id, fused_score, rerank_score)` triples
    /// (already ranked, already truncated to the caller's desired top-k)
    /// into full [`Hit`]s by looking up each chunk's stored meta. Ids whose
    /// meta is missing are skipped. Shared by [`Self::query`] and
    /// [`Self::vector_search`] via [`Self::apply_rerank`].
    fn resolve_hits(&self, ranked: Vec<(String, f64, Option<f32>)>) -> Result<Vec<Hit>> {
        let read_txn = self.meta.begin_read()?;
        let chunk_meta = read_txn.open_table(CHUNK_META)?;

        let mut hits = Vec::with_capacity(ranked.len());
        for (chunk_id, score, rerank_score) in ranked {
            let Some(encoded) = chunk_meta
                .get(chunk_id.as_str())?
                .map(|v| v.value().to_string())
            else {
                continue;
            };
            let record: ChunkMeta = serde_json::from_str(&encoded)?;
            hits.push(Hit {
                chunk_id,
                doc_path: record.doc_path,
                heading_path: record.heading_path,
                score,
                snippet: truncate_snippet(&record.text, SNIPPET_MAX_CHARS),
                rerank_score,
            });
        }
        Ok(hits)
    }

    /// Full stored text for each candidate chunk, index-aligned with `ids`
    /// (missing meta yields an empty string so reranker scores stay aligned).
    fn chunk_texts(&self, ids: &[(String, f64)]) -> Result<Vec<String>> {
        let read_txn = self.meta.begin_read()?;
        let chunk_meta = read_txn.open_table(CHUNK_META)?;
        let mut texts = Vec::with_capacity(ids.len());
        for (chunk_id, _) in ids {
            let text = match chunk_meta.get(chunk_id.as_str())? {
                // Deliberately default-not-propagate: a bad passage must not
                // fail the whole query (skip-not-fail), but corruption is a
                // real data signal — say so instead of silently demoting the
                // hit with a near-zero rerank score.
                Some(v) => match serde_json::from_str::<ChunkMeta>(v.value()) {
                    Ok(m) => m.text,
                    Err(e) => {
                        // Same rate-limit philosophy as the rerank-failure
                        // log: first corruption is a loud diagnostic, the
                        // rest stay quiet (widespread corruption would
                        // otherwise spam a daemon's stderr on every query).
                        if !self.chunk_corruption_logged.get() {
                            self.chunk_corruption_logged.set(true);
                            eprintln!(
                                "onebrain-search: chunk meta for {chunk_id} is corrupt ({e}) — \
                                 passage blanked for reranking; a `reindex --force` rebuilds it \
                                 (further corrupt-chunk warnings on this engine will not be logged)"
                            );
                        }
                        String::new()
                    }
                },
                // Missing meta = stale index entry; resolve_hits skips these
                // silently by design, so stay quiet here too.
                None => String::new(),
            };
            texts.push(text);
        }
        Ok(texts)
    }

    /// Tier-2 rerank stage over an already-fused ranking (ADR 0025):
    /// cross-encode the first `max(min_candidates, top_k)` entries against
    /// `query` — the reranked pool is a FLOOR of `min_candidates` that
    /// expands to cover `top_k`, never a ceiling, so every result the caller
    /// actually returns is always reranked — sort that block by calibrated
    /// score, gate at `min_score` (never-empty: total rejection keeps the top
    /// [`RERANK_NO_MATCH_KEEP`]), append the un-reranked fused tail (which
    /// trim-to-`top_k` then drops, since the block already covers the full
    /// returned set), trim to `top_k`, resolve. Skip-not-fail: no reranker →
    /// fused order with `rerank_score: None`; a rerank error falls back the
    /// same way — queries never fail because reranking did.
    fn apply_rerank(
        &self,
        query: &str,
        mut fused: Vec<(String, f64)>,
        top_k: usize,
    ) -> Result<Vec<Hit>> {
        let Some(reranker) = self.reranker() else {
            let mut unreranked: Vec<(String, f64, Option<f32>)> =
                fused.into_iter().map(|(id, s)| (id, s, None)).collect();
            unreranked.truncate(top_k);
            return self.resolve_hits(unreranked);
        };

        // `min_candidates` is a FLOOR, not a ceiling: every result the caller
        // actually returns (`top_k`) must be reranked, so the reranked pool is
        // max(min_candidates, top_k) — auto-raised whenever top_k exceeds the
        // configured floor. Only the fused tail beyond that
        // (already-truncated-away by `top_k` at the end of this function)
        // stays unreranked.
        let block_len = fused
            .len()
            .min(self.rerank_settings.min_candidates.max(top_k));
        let tail = fused.split_off(block_len);
        let head = fused;

        // The tokenizer enforces the model's hard context limit; this char
        // cap (~4 chars/token upper bound) just keeps huge chunks from
        // dominating tokenization time.
        let max_chars = rerank::reranker_registry()
            .iter()
            .find(|m| m.name == self.rerank_settings.model)
            .map(|m| m.max_length * 4)
            .unwrap_or(2048);
        let texts: Vec<String> = self
            .chunk_texts(&head)?
            .into_iter()
            .map(|t| t.chars().take(max_chars).collect())
            .collect();

        let mut ranked: Vec<(String, f64, Option<f32>)> = match reranker.rerank(query, &texts) {
            Ok(scores) => {
                let mut block: Vec<(String, f64, Option<f32>)> = head
                    .into_iter()
                    .zip(scores)
                    .map(|((id, fused_score), s)| (id, fused_score, Some(s)))
                    .collect();
                // Sort by cross-encoder score descending (total_cmp: no NaN
                // panic), ties by chunk_id for determinism — matching
                // `rrf_fuse`'s tie-break convention.
                block.sort_by(|a, b| {
                    b.2.unwrap_or(0.0)
                        .total_cmp(&a.2.unwrap_or(0.0))
                        .then_with(|| a.0.cmp(&b.0))
                });
                // The block is sorted, so gate survivors form its prefix.
                let min_score = self.rerank_settings.min_score;
                let survivors = block
                    .iter()
                    .take_while(|(_, _, s)| s.unwrap_or(0.0) >= min_score)
                    .count();
                let keep = if survivors == 0 {
                    RERANK_NO_MATCH_KEEP.min(block.len())
                } else {
                    survivors
                };
                block.truncate(keep);
                block
                    .into_iter()
                    .chain(tail.into_iter().map(|(id, s)| (id, s, None)))
                    .collect()
            }
            Err(e) => {
                // Log the first runtime failure per engine instance, then go
                // quiet: a persistently failing model would otherwise spam a
                // long-running daemon's stderr on every query. The unreranked
                // state itself stays visible per-query via rerank_score: None
                // (the CLI hint layer surfaces it).
                if !self.rerank_error_logged.get() {
                    self.rerank_error_logged.set(true);
                    eprintln!(
                        "onebrain-search: rerank failed — falling back to fused order \
                         (further rerank failures on this engine will not be logged): {e:#}"
                    );
                }
                head.into_iter()
                    .chain(tail)
                    .map(|(id, s)| (id, s, None))
                    .collect()
            }
        };
        ranked.truncate(top_k);
        self.resolve_hits(ranked)
    }

    /// Public, chunk-level entry point onto the Tier-2 rerank stage: rerank
    /// an already-fused `(chunk_id, fused_score)` ranking against `query` and
    /// resolve to `top_k` [`Hit`]s. Thin wrapper over [`Self::apply_rerank`]
    /// — callers (e.g. the MCP query-rerank tool) that already have a fused
    /// candidate list get the same gate + never-empty + skip-not-fail
    /// behavior as [`Self::query`]/[`Self::vector_search`] without
    /// reimplementing it.
    pub fn rerank_hits(
        &self,
        query: &str,
        fused: Vec<(String, f64)>,
        top_k: usize,
    ) -> Result<Vec<Hit>> {
        self.apply_rerank(query, fused, top_k)
    }

    /// Chunk ids belonging to `doc_path`, via the `DOC_CHUNKS` reverse index
    /// (doc_path → chunk_ids) already maintained by `index_doc`/`remove_doc`.
    /// Empty if `doc_path` is absent.
    fn doc_chunk_ids(&self, doc_path: &str) -> Result<Vec<String>> {
        let read_txn = self.meta.begin_read()?;
        let doc_chunks = read_txn.open_table(DOC_CHUNKS)?;
        match doc_chunks.get(doc_path)? {
            Some(v) => Ok(serde_json::from_str(v.value())?),
            None => Ok(Vec::new()),
        }
    }

    /// Doc-level rerank: given a set of candidate `paths` (e.g. from a prior
    /// path-scoped retrieval step), rerank a BOUNDED set of their chunks
    /// against `query`, then collapse down to one [`Hit`] per path — its
    /// best-scoring reranked chunk — and return the top `top_k` paths.
    ///
    /// **Bounded cost.** Reranking *every* chunk of *every* candidate path
    /// blows up on long-document vaults (a few candidate docs can be 100+
    /// chunks), far exceeding the chunk-level [`Self::rerank_hits`] path. This
    /// instead cross-encodes at most `budget = max(min_candidates, top_k)`
    /// chunks — the same pool size [`Self::apply_rerank`] bounds `rerank_hits`
    /// to — gathered ROUND-ROBIN across the candidate paths (every path's 1st
    /// chunk, then every path's 2nd, …). So each candidate contributes at
    /// least its first chunk before any path contributes a second: a long doc
    /// cannot crowd the budget and leave other candidates unreranked, and the
    /// total cross-encode cost is independent of document length.
    ///
    /// Reuses [`Self::apply_rerank`] for scoring, so it inherits the same gate,
    /// never-empty floor, and skip-not-fail fallback: with no reranker
    /// available this degenerates to fused (insertion) order with
    /// `rerank_score: None`, same as [`Self::rerank_hits`].
    pub fn rerank_paths(&self, query: &str, paths: Vec<String>, top_k: usize) -> Result<Vec<Hit>> {
        // Chunk ids per candidate path (order preserved for the round-robin).
        let per_path: Vec<Vec<String>> = paths
            .iter()
            .map(|p| self.doc_chunk_ids(p))
            .collect::<Result<Vec<_>>>()?;

        // Round-robin gather up to `budget` chunks so every candidate path is
        // represented (its first chunk) before any path adds a second — bounds
        // the cross-encode cost regardless of how long the candidate docs are.
        let budget = self.rerank_settings.min_candidates.max(top_k);
        let mut fused: Vec<(String, f64)> = Vec::new();
        let mut round = 0;
        'gather: loop {
            let mut progressed = false;
            for chunks in &per_path {
                if let Some(chunk_id) = chunks.get(round) {
                    fused.push((chunk_id.clone(), 0.0));
                    progressed = true;
                    if fused.len() >= budget {
                        break 'gather;
                    }
                }
            }
            if !progressed {
                break; // every path exhausted before reaching the budget
            }
            round += 1;
        }

        // `fused.len() <= budget`, so passing `budget` as top_k keeps every
        // gathered chunk alive through apply_rerank's internal block cap — the
        // doc-level `top_k` truncation happens below, after dedup.
        let reranked = self.apply_rerank(query, fused, budget)?;

        // Dedup by doc_path, keeping the highest-`rerank_score` hit per path
        // (None treated as lowest — matches apply_rerank's own tie/absent
        // handling). `reranked` is already sorted best-first, so the first
        // occurrence of each path is its best chunk; preserve that order.
        let mut best_by_path: std::collections::HashMap<String, Hit> =
            std::collections::HashMap::new();
        let mut order: Vec<String> = Vec::new();
        for hit in reranked {
            let key = hit.doc_path.clone();
            match best_by_path.get(&key) {
                Some(existing)
                    if existing.rerank_score.unwrap_or(f32::MIN)
                        >= hit.rerank_score.unwrap_or(f32::MIN) =>
                {
                    // Existing entry already scores at least as high; drop this one.
                }
                _ => {
                    if !order.contains(&key) {
                        order.push(key.clone());
                    }
                    best_by_path.insert(key, hit);
                }
            }
        }

        let mut deduped: Vec<Hit> = order
            .into_iter()
            .filter_map(|path| best_by_path.remove(&path))
            .collect();
        deduped.truncate(top_k);
        Ok(deduped)
    }

    /// Hybrid search: lex + vec (top ~50 each) fused via RRF, then passed
    /// through the Tier-2 rerank stage ([`Self::apply_rerank`]), resolved to
    /// `top_k` [`Hit`]s. Fused ids whose meta is missing are skipped.
    pub fn query(&self, text: &str, top_k: usize) -> Result<Vec<Hit>> {
        let query_vec = self.embedder()?.embed_query(text)?;
        // Recall-first: keep the vec top cluster (relative) and let RRF + the
        // rerank stage provide precision. The old absolute floor silently
        // dropped ALL vec hits for e5 (scores ~0.83–0.87 < 0.85), degrading
        // hybrid to lex-only.
        let vec_hits = keep_top_cluster(self.vec.search(&query_vec, VEC_TOP_K), VEC_CLUSTER_WINDOW);
        let lex_hits = self.lex.search(text, LEX_TOP_K)?;

        // Fuse wide enough for the rerank stage to see its full reranked
        // pool even when the caller asks for a small top_k.
        let fuse_k = top_k.max(self.rerank_settings.min_candidates);
        let fused = rrf_fuse(&lex_hits, &vec_hits, RRF_K, fuse_k);
        self.apply_rerank(text, fused, top_k)
    }

    /// Vector-only semantic search (no lex/RRF fusion): embed `text`, take
    /// the top nearest chunks by cosine similarity, then pass them through
    /// the Tier-2 rerank stage ([`Self::apply_rerank`]). Used by the CLI's
    /// `search vsearch` verb.
    pub fn vector_search(&self, text: &str, top_k: usize) -> Result<Vec<Hit>> {
        let query_vec = self.embedder()?.embed_query(text)?;
        let fetch_k = top_k.max(self.rerank_settings.min_candidates);
        let vec_hits = keep_top_cluster(self.vec.search(&query_vec, fetch_k), VEC_CLUSTER_WINDOW);
        let ranked: Vec<(String, f64)> = vec_hits
            .into_iter()
            .map(|(id, score)| (id, score as f64))
            .collect();
        self.apply_rerank(text, ranked, top_k)
    }

    /// Full stored text of a doc (its chunks concatenated in
    /// `chunk_index` order), or an error if `doc_path` is absent.
    pub fn get(&self, doc_path: &str) -> Result<String> {
        let read_txn = self.meta.begin_read()?;
        let doc_chunks = read_txn.open_table(DOC_CHUNKS)?;
        let encoded = doc_chunks
            .get(doc_path)?
            .with_context(|| format!("doc not found: {doc_path}"))?
            .value()
            .to_string();
        let chunk_ids: Vec<String> = serde_json::from_str(&encoded)?;

        let chunk_meta = read_txn.open_table(CHUNK_META)?;
        let mut records: Vec<ChunkMeta> = Vec::with_capacity(chunk_ids.len());
        for chunk_id in &chunk_ids {
            if let Some(v) = chunk_meta.get(chunk_id.as_str())? {
                records.push(serde_json::from_str(v.value())?);
            }
        }
        records.sort_by_key(|r| r.chunk_index);

        Ok(records
            .into_iter()
            .map(|r| r.text)
            .collect::<Vec<_>>()
            .join("\n\n"))
    }

    /// Stored content hash for `doc_path`, if any.
    fn stored_hash(&self, doc_path: &str) -> Result<Option<String>> {
        let read_txn = self.meta.begin_read()?;
        let doc_hashes = read_txn.open_table(DOC_HASHES)?;
        Ok(doc_hashes.get(doc_path)?.map(|v| v.value().to_string()))
    }

    /// Store `hash` as `doc_path`'s content hash.
    fn store_hash(&mut self, doc_path: &str, hash: &str) -> Result<()> {
        let write_txn = self.meta.begin_write()?;
        {
            let mut doc_hashes = write_txn.open_table(DOC_HASHES)?;
            doc_hashes.insert(doc_path, hash)?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Stored `LEX_HASHES` entry for `doc_path`, if any (no `DOC_HASHES`
    /// fallback — see [`Engine::effective_lex_hash`] for the combined read).
    fn stored_lex_hash(&self, doc_path: &str) -> Result<Option<String>> {
        let read_txn = self.meta.begin_read()?;
        let lex_hashes = read_txn.open_table(LEX_HASHES)?;
        Ok(lex_hashes.get(doc_path)?.map(|v| v.value().to_string()))
    }

    /// True if `doc_path` is recorded in EITHER `DOC_HASHES` or `LEX_HASHES`,
    /// read in a SINGLE transaction (vs `stored_hash` + `stored_lex_hash` =
    /// two `begin_read`s). Used on the removal path, where presence in either
    /// table means the doc was indexed and must be removed.
    fn is_indexed_in_either(&self, doc_path: &str) -> Result<bool> {
        let read_txn = self.meta.begin_read()?;
        let doc_hashes = read_txn.open_table(DOC_HASHES)?;
        if doc_hashes.get(doc_path)?.is_some() {
            return Ok(true);
        }
        let lex_hashes = read_txn.open_table(LEX_HASHES)?;
        Ok(lex_hashes.get(doc_path)?.is_some())
    }

    /// The hash a lex-only reindex should diff against: `LEX_HASHES` if
    /// present, else `DOC_HASHES`. A fully-indexed (`Full`-mode) doc is by
    /// definition lex-indexed too, so falling back to `DOC_HASHES` makes
    /// existing indexes (built before `LEX_HASHES` existed) work correctly
    /// with zero migration — they simply read as already lex-current.
    fn effective_lex_hash(&self, doc_path: &str) -> Result<Option<String>> {
        if let Some(h) = self.stored_lex_hash(doc_path)? {
            return Ok(Some(h));
        }
        self.stored_hash(doc_path)
    }

    /// Store `hash` as `doc_path`'s `LEX_HASHES` entry (lex-only reindex —
    /// never touches `DOC_HASHES`).
    fn store_lex_hash(&mut self, doc_path: &str, hash: &str) -> Result<()> {
        let write_txn = self.meta.begin_write()?;
        {
            let mut lex_hashes = write_txn.open_table(LEX_HASHES)?;
            lex_hashes.insert(doc_path, hash)?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Drop `doc_path`'s `LEX_HASHES` entry, if any. Used by a `Full`-mode
    /// reindex once it has re-indexed a doc: after that, `DOC_HASHES` alone
    /// is authoritative again (see [`Engine::effective_lex_hash`]), and a
    /// stale `LEX_HASHES` entry left behind could otherwise diverge from it.
    fn drop_lex_hash(&mut self, doc_path: &str) -> Result<()> {
        let write_txn = self.meta.begin_write()?;
        {
            let mut lex_hashes = write_txn.open_table(LEX_HASHES)?;
            lex_hashes.remove(doc_path)?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Record `epoch_secs` as the index's `last_indexed_at` in the engine
    /// header. Called by the reindex paths at the end of a successful run.
    fn record_last_indexed(&mut self, epoch_secs: u64) -> Result<()> {
        let write_txn = self.meta.begin_write()?;
        {
            let mut header = write_txn.open_table(ENGINE_HEADER)?;
            header.insert(LAST_INDEXED_KEY, epoch_secs.to_string().as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Stored `last_indexed_at` epoch seconds, if a reindex has ever run.
    /// Unparseable values are treated as absent (defensive — the field is
    /// only ever written as a decimal string by [`Self::record_last_indexed`]).
    fn stored_last_indexed(&self) -> Result<Option<u64>> {
        let read_txn = self.meta.begin_read()?;
        let header = read_txn.open_table(ENGINE_HEADER)?;
        Ok(header
            .get(LAST_INDEXED_KEY)?
            .and_then(|v| v.value().parse::<u64>().ok()))
    }

    /// The single choke-point (design §3a / §8 risk row) that bumps the
    /// [`GENERATION_KEY`] counter by one, in ONE atomic write txn. Every
    /// reindex mode calls exactly this — full and lex-only — so no reindex
    /// path can forget to invalidate the memo cache.
    ///
    /// **Crash safety (MAJOR 1).** The counter lives in `engine.redb` (`meta`)
    /// while the actual index content lives in tantivy + the vector store —
    /// SEPARATE durable stores that are NOT committed together, so a crash
    /// mid-reindex can leave them at different points. To make that safe the
    /// callers bump BEFORE mutating the index (and again after — see the
    /// reindex bodies): a crash then leaves `generation` already advanced while
    /// the index is old/partial, so a memo key built with the new generation
    /// MISSES and the result is recomputed. The reverse ordering (bump last)
    /// would leave `generation` old against a new index — a stale hit, the one
    /// outcome the memo layer must make impossible. Over-invalidating (a bump
    /// with no matching index change) is always safe: it only costs a cache
    /// miss.
    fn bump_generation(&mut self) -> Result<()> {
        let write_txn = self.meta.begin_write()?;
        {
            let mut header = write_txn.open_table(ENGINE_HEADER)?;
            let current = header
                .get(GENERATION_KEY)?
                .and_then(|v| v.value().parse::<u64>().ok())
                .unwrap_or(0);
            let next = current.wrapping_add(1);
            header.insert(GENERATION_KEY, next.to_string().as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// The current index generation — a monotonic counter bumped once per
    /// reindex (full or lex-only). `0` before any reindex has run (or if the
    /// header can't be read — treated as "never indexed", the safe default
    /// that just misses the memo cache rather than serving staleness). The
    /// memoization cache (Track 3) folds this into every key so that a reindex
    /// invalidates all prior entries by construction.
    pub fn generation(&self) -> u64 {
        (|| -> Result<u64> {
            let read_txn = self.meta.begin_read()?;
            let header = read_txn.open_table(ENGINE_HEADER)?;
            Ok(header
                .get(GENERATION_KEY)?
                .and_then(|v| v.value().parse::<u64>().ok())
                .unwrap_or(0))
        })()
        .unwrap_or(0)
    }

    /// The stable per-index-instance nonce (design §3a / MAJOR 2). Constant for
    /// the life of one `engine.redb`; changes only when the index is created
    /// fresh (a `--force`/wipe/`rm index/` that drops the header). The memo
    /// cache folds this into every key alongside [`Engine::generation`] so a
    /// rebuilt index — whose `generation` restarts at 0→1 and would collide
    /// with stale entries stored under the old instance's generation 1 — gets a
    /// disjoint key space instead. Empty string only if the header can't be
    /// read (treated as "unknown instance" — a memo miss, never a false hit).
    pub fn index_nonce(&self) -> String {
        (|| -> Result<Option<String>> {
            let read_txn = self.meta.begin_read()?;
            let header = read_txn.open_table(ENGINE_HEADER)?;
            Ok(header.get(INDEX_NONCE_KEY)?.map(|v| v.value().to_string()))
        })()
        .ok()
        .flatten()
        .unwrap_or_default()
    }

    /// Public content-hash accessor for `doc_path` (design §3b) — the
    /// already-sent ledger (Track 3) needs to compare a doc's current content
    /// hash without re-hashing large bodies. Reuses the stored hashes: the
    /// `LEX_HASHES` entry if present, else `DOC_HASHES` (via
    /// [`Engine::effective_lex_hash`]), so a lex-only-indexed doc resolves too.
    /// `None` for an unknown/unindexed doc (or on any read error — the caller
    /// then simply treats the doc as first-send, never a false "unchanged").
    pub fn doc_hash(&self, doc_path: &str) -> Option<String> {
        self.effective_lex_hash(doc_path).ok().flatten()
    }

    /// Drop `doc_path`'s stored content hash from BOTH `DOC_HASHES` and
    /// `LEX_HASHES`. Used when a doc is swept as removed (file gone from
    /// disk) — in both `Full` and `LexOnly` reindex modes a removed doc must
    /// stop being reported as drift entirely, not merely fall back from one
    /// table to the other.
    fn drop_hash(&mut self, doc_path: &str) -> Result<()> {
        let write_txn = self.meta.begin_write()?;
        {
            let mut doc_hashes = write_txn.open_table(DOC_HASHES)?;
            doc_hashes.remove(doc_path)?;
        }
        {
            let mut lex_hashes = write_txn.open_table(LEX_HASHES)?;
            lex_hashes.remove(doc_path)?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Reindex one doc (already known to exist on disk at `abs_path`)
    /// against its stored hash, updating `stats` in place.
    ///
    /// `on_first_embed` is invoked exactly once across a whole run — right
    /// before the first embed call (the first Added / Updated doc). Callers
    /// pass a closure that emits [`ReindexProgress::LoadingModel`] and flips a
    /// shared "already announced" flag so it fires at most once. Unchanged docs
    /// don't embed, so they never trigger it. In [`IndexMode::LexOnly`],
    /// `on_first_embed` is never called at all — nothing is ever embedded.
    ///
    /// Mode behavior:
    /// - [`IndexMode::Full`]: diff against `DOC_HASHES`; on Added/Updated,
    ///   `remove_doc` (unconditionally — a no-op for a brand-new doc, but
    ///   required on Added too since a prior lex-only pass may have already
    ///   lex-indexed this doc under LEX_HASHES without a DOC_HASHES entry;
    ///   "no DOC_HASHES ⟹ no lex entries" stopped holding once lex-only mode
    ///   was introduced), `index_doc` (full), store into `DOC_HASHES`, and
    ///   drop any `LEX_HASHES` entry (keeps [`Engine::effective_lex_hash`]'s
    ///   fallback correct — see that function's doc comment).
    /// - [`IndexMode::LexOnly`]: diff against the *effective* lex hash
    ///   ([`Engine::effective_lex_hash`]); on Added/Updated, `remove_doc`
    ///   first (drops any stale lex+vec+meta; a no-op for brand-new docs),
    ///   then `index_doc_mode(LexOnly)`, and store into `LEX_HASHES` ONLY —
    ///   `DOC_HASHES` and `on_first_embed` are never touched, so
    ///   [`Engine::status`] keeps reporting the doc as pending until a real
    ///   (Full) embed pass runs.
    fn reindex_existing_doc(
        &mut self,
        doc_path: &str,
        abs_path: &Path,
        mode: IndexMode,
        stats: &mut ReindexStats,
        on_first_embed: &mut dyn FnMut(),
    ) -> Result<()> {
        let bytes =
            std::fs::read(abs_path).with_context(|| format!("reading {}", abs_path.display()))?;
        let current_hash = hash_bytes(&bytes);

        let stored = match mode {
            IndexMode::Full => self.stored_hash(doc_path)?,
            IndexMode::LexOnly => self.effective_lex_hash(doc_path)?,
        };

        match diff_hash(stored.as_deref(), &current_hash) {
            HashDiff::Unchanged => {
                stats.unchanged += 1;
            }
            HashDiff::Added => {
                let content = String::from_utf8_lossy(&bytes).into_owned();
                match mode {
                    IndexMode::Full => {
                        on_first_embed();
                        // `remove_doc` here even though this is the `Added`
                        // branch: a prior lex-only pass may have already
                        // lex-indexed this doc (LEX_HASHES set, DOC_HASHES
                        // never touched), so "no DOC_HASHES ⟹ no lex
                        // entries" no longer holds now that lex-only mode
                        // exists. Without this, `index_doc`'s deterministic
                        // chunk_ids would collide with the still-present
                        // lex-only tantivy docs (`LexIndex::add` never
                        // deletes-first), duplicating lex hits. `remove_doc`
                        // is keyed off `DOC_CHUNKS`, which lex-only indexing
                        // also populates, so it finds and clears those
                        // chunks; for a truly brand-new doc it's a no-op.
                        self.remove_doc(doc_path)?;
                        self.index_doc(doc_path, &content)?;
                        self.store_hash(doc_path, &current_hash)?;
                        self.drop_lex_hash(doc_path)?;
                    }
                    IndexMode::LexOnly => {
                        self.remove_doc(doc_path)?;
                        self.index_doc_mode(doc_path, &content, IndexMode::LexOnly)?;
                        self.store_lex_hash(doc_path, &current_hash)?;
                    }
                }
                stats.added += 1;
            }
            HashDiff::Updated => {
                let content = String::from_utf8_lossy(&bytes).into_owned();
                match mode {
                    IndexMode::Full => {
                        on_first_embed();
                        self.remove_doc(doc_path)?;
                        self.index_doc(doc_path, &content)?;
                        self.store_hash(doc_path, &current_hash)?;
                        self.drop_lex_hash(doc_path)?;
                    }
                    IndexMode::LexOnly => {
                        self.remove_doc(doc_path)?;
                        self.index_doc_mode(doc_path, &content, IndexMode::LexOnly)?;
                        self.store_lex_hash(doc_path, &current_hash)?;
                    }
                }
                stats.updated += 1;
            }
        }
        Ok(())
    }

    /// Reindex specific docs. `doc_paths` are vault-relative paths (also
    /// the meta keys). For each: read `vault_root.join(doc_path)`, sha256
    /// its bytes, and compare to the stored hash — see the module-level
    /// task doc for the four cases (added/updated/unchanged/removed).
    pub fn reindex_paths(
        &mut self,
        vault_root: &Path,
        doc_paths: &[String],
    ) -> Result<ReindexStats> {
        self.reindex_paths_with_progress(vault_root, doc_paths, &mut |_| {})
    }

    /// Like [`Engine::reindex_paths`] but reports live [`ReindexProgress`]
    /// events through `progress`. See [`Engine::reindex_all_with_progress`] for
    /// the emission contract (`LoadingModel` once before the first embed, then
    /// `Indexing` after each doc).
    pub fn reindex_paths_with_progress(
        &mut self,
        vault_root: &Path,
        doc_paths: &[String],
        progress: &mut dyn FnMut(ReindexProgress),
    ) -> Result<ReindexStats> {
        self.reindex_paths_with_progress_inner(vault_root, doc_paths, IndexMode::Full, progress)
    }

    /// Lex-only counterpart to [`Engine::reindex_paths_with_progress`]: for
    /// each targeted doc, updates lex + chunk meta and `LEX_HASHES` only —
    /// the embedder is never touched, and `DOC_HASHES` is left exactly as it
    /// was, so [`Engine::status`] keeps reporting these docs as pending until
    /// a later full/pending embed pass runs. See the module-level doc comment
    /// on [`Engine::reindex_all_lex_only_with_progress`] for the full
    /// rationale (this function shares its mode plumbing).
    pub fn reindex_paths_lex_only_with_progress(
        &mut self,
        vault_root: &Path,
        doc_paths: &[String],
        progress: &mut dyn FnMut(ReindexProgress),
    ) -> Result<ReindexStats> {
        self.reindex_paths_with_progress_inner(vault_root, doc_paths, IndexMode::LexOnly, progress)
    }

    /// Shared body of [`Engine::reindex_paths_with_progress`] /
    /// [`Engine::reindex_paths_lex_only_with_progress`]: only `mode` differs.
    /// NOTE: unlike [`Engine::reindex_all_with_progress_inner`]'s sweep, a
    /// targeted removal here is scoped to `doc_paths` (a doc not passed in is
    /// simply not considered), so `drop_hash` (both tables) applies per-path
    /// exactly as before lex-only existed.
    fn reindex_paths_with_progress_inner(
        &mut self,
        vault_root: &Path,
        doc_paths: &[String],
        mode: IndexMode,
        progress: &mut dyn FnMut(ReindexProgress),
    ) -> Result<ReindexStats> {
        let total = doc_paths.len();
        progress(ReindexProgress::Walked { total });
        // MAJOR 1 (crash safety): bump BEFORE any index mutation. A crash
        // mid-loop then leaves generation advanced against an old/partial
        // index → a new-gen memo key misses → safe recompute. See
        // [`Engine::bump_generation`].
        self.bump_generation()?;
        let mut model_announced = false;
        let mut stats = ReindexStats::default();
        for (i, doc_path) in doc_paths.iter().enumerate() {
            // Defense-in-depth (#175): confine each caller-supplied path to the
            // vault BEFORE `join`/`read`. The HTTP layer already confines, but
            // the DIRECT path (`ONEBRAIN_NO_DAEMON=1`, daemon fallback, plain
            // `search reindex <paths>`) reaches here unchecked. A path that
            // escapes is skipped (counted as `failed`), never read/indexed.
            if let Err(reason) = confine_reindex_doc_path(vault_root, doc_path) {
                stats.failed += 1;
                eprintln!("onebrain-search: skipping {doc_path}: {reason}");
                progress(ReindexProgress::Indexing {
                    done: i + 1,
                    total,
                    doc_path: doc_path.clone(),
                });
                continue;
            }
            let abs_path = vault_root.join(doc_path);
            if abs_path.is_file() {
                let mut on_first_embed = || {
                    if !model_announced {
                        model_announced = true;
                        progress(ReindexProgress::LoadingModel);
                    }
                };
                self.reindex_existing_doc(
                    doc_path,
                    &abs_path,
                    mode,
                    &mut stats,
                    &mut on_first_embed,
                )?;
            } else if self.is_indexed_in_either(doc_path)? {
                self.remove_doc(doc_path)?;
                self.drop_hash(doc_path)?;
                stats.removed += 1;
            }
            // Neither on disk nor indexed: ignore.
            progress(ReindexProgress::Indexing {
                done: i + 1,
                total,
                doc_path: doc_path.clone(),
            });
        }
        // Lex-only runs never touch `last_indexed_at`: that field means
        // "vectors are current as of", and a lex-only run never embeds, so
        // recording it here would make `status` under-report drift.
        if mode == IndexMode::Full {
            self.record_last_indexed(now_epoch_secs())?;
        }
        // Second bump AFTER the index is fully updated: closes the
        // concurrent-query-during-reindex gap — a search that ran mid-reindex
        // and memoized against the partially-updated index (under the
        // pre-loop generation) is invalidated here. Over-invalidation is safe.
        self.bump_generation()?;
        Ok(stats)
    }

    /// Reindex the whole vault: walk `vault_root` for `*.md` files
    /// (recursively), reindex each, and remove any previously-indexed doc
    /// whose file no longer exists.
    pub fn reindex_all(&mut self, vault_root: &Path) -> Result<ReindexStats> {
        self.reindex_all_with_progress(vault_root, &mut |_| {})
    }

    /// Like [`Engine::reindex_all`] but reports live [`ReindexProgress`] events
    /// through `progress` so a caller can render a progress bar.
    ///
    /// Emission contract:
    /// - [`ReindexProgress::LoadingModel`] fires at most once, right before the
    ///   first embed call (the first Added / Updated doc) — the model-download
    ///   point on a first index. A run that only sees unchanged / removed docs
    ///   never emits it.
    /// - [`ReindexProgress::Indexing`] fires once per walked doc after it's
    ///   processed, with `done` counting up to `total` (the walked file count).
    ///
    /// The trailing stale-doc sweep (files gone from disk) is not part of the
    /// walked `total` and does not emit progress — it's bounded by prior index
    /// state, not the current walk, and typically empty.
    pub fn reindex_all_with_progress(
        &mut self,
        vault_root: &Path,
        progress: &mut dyn FnMut(ReindexProgress),
    ) -> Result<ReindexStats> {
        self.reindex_all_with_progress_inner(vault_root, IndexMode::Full, progress)
    }

    /// Lex-only counterpart to [`Engine::reindex_all_with_progress`]: walks
    /// the whole vault and updates the lex/BM25 index + chunk meta for every
    /// added/updated doc, but the embedder is NEVER constructed or called —
    /// not even the lazy production embedder in a `semantic` build. Changed
    /// docs are recorded into `LEX_HASHES`, NOT `DOC_HASHES`, so
    /// [`Engine::status`] keeps reporting them as pending: a lex-indexed doc
    /// is deliberately indistinguishable from an un-embedded one until a real
    /// (Full) reindex/embed pass runs. [`ReindexProgress::LoadingModel`] is
    /// therefore never emitted by a lex-only run — there is nothing to load.
    ///
    /// The trailing stale-doc sweep (file gone from disk) behaves exactly as
    /// in [`Engine::reindex_all_with_progress`]: `remove_doc` plus dropping
    /// BOTH hash-table entries via [`Engine::drop_hash`], since a gone file
    /// should stop counting as drift under either hash source.
    ///
    /// `last_indexed_at` is NOT updated by a lex-only run: that field means
    /// "vectors are current as of this time", which is exactly what did NOT
    /// just happen.
    pub fn reindex_all_lex_only_with_progress(
        &mut self,
        vault_root: &Path,
        progress: &mut dyn FnMut(ReindexProgress),
    ) -> Result<ReindexStats> {
        self.reindex_all_with_progress_inner(vault_root, IndexMode::LexOnly, progress)
    }

    /// Shared body of [`Engine::reindex_all_with_progress`] /
    /// [`Engine::reindex_all_lex_only_with_progress`]: only `mode` differs
    /// (which hash table is diffed/written, and whether the embedder is ever
    /// touched — see [`Engine::reindex_existing_doc`]).
    fn reindex_all_with_progress_inner(
        &mut self,
        vault_root: &Path,
        mode: IndexMode,
        progress: &mut dyn FnMut(ReindexProgress),
    ) -> Result<ReindexStats> {
        let files = walk_markdown_files(vault_root, &self.exclude_patterns)?;
        let doc_paths: Vec<String> = files
            .iter()
            .filter_map(|f| vault_relative_path(vault_root, f))
            .collect();

        let total = doc_paths.len();
        progress(ReindexProgress::Walked { total });
        // MAJOR 1 (crash safety): bump BEFORE any index mutation — see the
        // paths-inner counterpart and [`Engine::bump_generation`].
        self.bump_generation()?;
        let mut model_announced = false;
        let mut stats = ReindexStats::default();
        for (i, (doc_path, abs_path)) in doc_paths.iter().zip(files.iter()).enumerate() {
            // A single unreadable/failing file must not abort the whole vault
            // reindex (walk_markdown_files already tolerates bad dirs — keep
            // the file loop consistent). Count the failure and continue.
            let mut on_first_embed = || {
                if !model_announced {
                    model_announced = true;
                    progress(ReindexProgress::LoadingModel);
                }
            };
            if let Err(e) =
                self.reindex_existing_doc(doc_path, abs_path, mode, &mut stats, &mut on_first_embed)
            {
                stats.failed += 1;
                eprintln!("onebrain-search: skipping {doc_path}: {e:#}");
            }
            progress(ReindexProgress::Indexing {
                done: i + 1,
                total,
                doc_path: doc_path.clone(),
            });
        }

        // Sweep: any stored hash whose doc_path wasn't just seen on disk
        // means the file is gone. Membership is tested against a HashSet built
        // once (O(N) total) rather than `Vec::contains` per entry (O(N²)).
        // Same in both modes: check BOTH hash tables and drop BOTH on removal
        // (drop_hash clears both), so a doc lex-indexed-only is swept just
        // like a fully-indexed one.
        let seen: std::collections::HashSet<&String> = doc_paths.iter().collect();
        let stale: Vec<String> = {
            let read_txn = self.meta.begin_read()?;
            let doc_hashes = read_txn.open_table(DOC_HASHES)?;
            let lex_hashes = read_txn.open_table(LEX_HASHES)?;
            let mut out: std::collections::HashSet<String> = std::collections::HashSet::new();
            for entry in doc_hashes.iter()? {
                let (k, _) = entry?;
                out.insert(k.value().to_string());
            }
            for entry in lex_hashes.iter()? {
                let (k, _) = entry?;
                out.insert(k.value().to_string());
            }
            out.into_iter().filter(|k| !seen.contains(k)).collect()
        };
        for doc_path in stale {
            self.remove_doc(&doc_path)?;
            self.drop_hash(&doc_path)?;
            stats.removed += 1;
        }

        // Second bump AFTER the index is fully updated (post-loop, post-sweep):
        // closes the concurrent-query-during-reindex gap (a search that
        // memoized against the partial index under the pre-loop generation is
        // invalidated here). Placed BEFORE the full-only reranker fetch below so
        // an early return on a fetch error can't skip it. Over-invalidation is
        // safe (a cache miss, never a stale hit).
        self.bump_generation()?;

        // Lex-only runs never touch `last_indexed_at`: see this function's
        // doc comment (and `reindex_all_lex_only_with_progress`'s) — it means
        // "vectors are current as of", which a lex-only run never makes true.
        if mode == IndexMode::Full {
            self.record_last_indexed(now_epoch_secs())?;

            // End-of-run reranker fetch: a lex-only pass never embeds, so it
            // has no business fetching the (embed-adjacent) reranker model
            // either. Downloaded-status is a plain filesystem check (never
            // feature-gated), so this is safe to evaluate in lex-only
            // builds too — `should_fetch_reranker` just comes back false
            // once the model directory exists, and an unknown model name is
            // treated as "downloaded" (nothing to fetch), mirroring
            // `build_reranker`'s skip-on-unknown-model.
            let downloaded = rerank::reranker_registry()
                .iter()
                .find(|m| m.name == self.rerank_settings.model)
                .map(|info| rerank::reranker_download_status(info, self.layout.root()).downloaded)
                .unwrap_or(true);
            if should_fetch_reranker(self.rerank_settings.enabled, downloaded) {
                progress(ReindexProgress::LoadingReranker);
                // Download the model NOW via the dedicated fetch path (hf-hub
                // prints its own progress). Must be `fetch_reranker_model`,
                // NOT `reranker()`: the lazy accessor skips when the model is
                // absent (queries must not trigger a 570MB download), so it
                // would never actually fetch here — the model could never
                // arrive. Skip-not-fail — a download error never fails the
                // reindex.
                self.fetch_reranker_model();
            }
        }
        Ok(stats)
    }

    /// Read-only index status: doc count, last-indexed timestamp, and the
    /// pending drift between the on-disk vault and the index.
    ///
    /// Never constructs the embedder — it only reads stored hashes and
    /// re-hashes the vault's `*.md` files (one sha256 per file). Safe to call
    /// before any model has been downloaded. The drift walk is the same
    /// add/update/unchanged/remove classification a reindex does, MINUS the
    /// indexing side-effects.
    pub fn status(&self, vault_root: &Path) -> Result<IndexStatus> {
        let drift = self.classify_doc_hashes_drift(vault_root)?;
        Ok(IndexStatus {
            doc_count: drift.doc_count,
            last_indexed_at: self.stored_last_indexed()?,
            pending_new: drift.added.len(),
            pending_changed: drift.changed.len(),
            pending_removed: drift.removed.len(),
        })
    }

    /// The exact doc-path worklist a deferred embed pass must process: docs
    /// on disk with no `DOC_HASHES` entry (new), docs whose stored
    /// `DOC_HASHES` hash differs from disk (changed), and docs with a
    /// `DOC_HASHES` entry whose file is gone (removed — included because
    /// `reindex_paths` handles removal for missing files too).
    ///
    /// `LEX_HASHES` plays no role here: pending is defined purely by
    /// `DOC_HASHES` drift, so a lex-only-indexed doc (see
    /// [`Engine::reindex_all_lex_only_with_progress`]) is still reported as
    /// pending until a real (Full) reindex/embed pass runs — same rule
    /// [`Engine::status`] uses.
    ///
    /// Never constructs the embedder. Order: added/changed docs in
    /// `walk_markdown_files` order, then removed docs sorted for stability.
    pub fn pending_vector_paths(&self, vault_root: &Path) -> Result<Vec<String>> {
        let drift = self.classify_doc_hashes_drift(vault_root)?;
        let mut out = drift.added;
        out.extend(drift.changed);
        let mut removed = drift.removed;
        removed.sort();
        out.extend(removed);
        Ok(out)
    }

    /// Shared walk-classify core of [`Engine::status`] and
    /// [`Engine::pending_vector_paths`]: walks `*.md` files under
    /// `vault_root` (honoring `exclude_patterns`), hashes each one, and
    /// classifies it against the stored `DOC_HASHES` table. Unreadable files
    /// are skipped (a reindex would count them as `failed`, not drift) to
    /// keep this read-only and resilient. `added`/`changed` are in walk
    /// order; `removed` is unordered (callers sort if they need stability).
    fn classify_doc_hashes_drift(&self, vault_root: &Path) -> Result<DocHashesDrift> {
        // Snapshot every stored (doc_path -> hash) once. Doc count is the
        // number of distinct stored hashes.
        let stored: std::collections::HashMap<String, String> = {
            let read_txn = self.meta.begin_read()?;
            let doc_hashes = read_txn.open_table(DOC_HASHES)?;
            let mut out = std::collections::HashMap::new();
            for entry in doc_hashes.iter()? {
                let (k, v) = entry?;
                out.insert(k.value().to_string(), v.value().to_string());
            }
            out
        };
        let doc_count = stored.len();

        let files = walk_markdown_files(vault_root, &self.exclude_patterns)?;
        let mut added = Vec::new();
        let mut changed = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for abs_path in &files {
            let Some(doc_path) = vault_relative_path(vault_root, abs_path) else {
                continue;
            };
            let bytes = match std::fs::read(abs_path) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let current_hash = hash_bytes(&bytes);
            match diff_hash(stored.get(&doc_path).map(String::as_str), &current_hash) {
                HashDiff::Added => added.push(doc_path.clone()),
                HashDiff::Updated => changed.push(doc_path.clone()),
                HashDiff::Unchanged => {}
            }
            seen.insert(doc_path);
        }

        // Indexed docs whose file is gone from disk (would be removed).
        let removed: Vec<String> = stored
            .keys()
            .filter(|k| !seen.contains(*k))
            .cloned()
            .collect();

        Ok(DocHashesDrift {
            doc_count,
            added,
            changed,
            removed,
        })
    }
}

/// Result of [`Engine::classify_doc_hashes_drift`]: the same
/// add/update/unchanged/remove classification a reindex does against
/// `DOC_HASHES`, minus the indexing side-effects. `doc_count` is the number
/// of distinct stored `DOC_HASHES` keys (matches [`IndexStatus::doc_count`]).
struct DocHashesDrift {
    doc_count: usize,
    added: Vec<String>,
    changed: Vec<String>,
    removed: Vec<String>,
}

/// Current wall-clock time as whole epoch seconds. Clamps a
/// before-epoch clock (never happens in practice) to 0 rather than
/// panicking.
fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Mint a fresh index-instance nonce ([`INDEX_NONCE_KEY`]). Combines
/// wall-clock nanoseconds with a process-wide monotonic counter so that two
/// index recreations — even back-to-back within the same nanosecond in a
/// test — never collide: the counter disambiguates within a process, the
/// nanos across process restarts.
fn fresh_index_nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NONCE_COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = NONCE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}-{seq:x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::Embed;

    /// A deterministic in-memory embedder for tests. No network, no model
    /// download — it hashes each whitespace-separated token into one of
    /// `dims` buckets, accumulates a count per bucket, then L2-normalizes.
    ///
    /// Properties the engine tests rely on:
    /// - identical input text → identical vector (deterministic);
    /// - distinct texts get distinguishable vectors (different token sets map
    ///   to different bucket distributions);
    /// - a chunk's exact text embeds to the same vector it was indexed with,
    ///   so cosine similarity is 1.0 and that chunk is the top vector hit for
    ///   a query using its text.
    struct FakeEmbedder {
        dims: usize,
    }

    impl FakeEmbedder {
        fn embed_one(&self, text: &str) -> Vec<f32> {
            let mut v = vec![0.0f32; self.dims];
            for token in text.split_whitespace() {
                // Simple, stable FNV-1a-ish hash over the token's bytes.
                let mut h: u64 = 1469598103934665603;
                for b in token.bytes() {
                    h ^= b as u64;
                    h = h.wrapping_mul(1099511628211);
                }
                let bucket = (h % self.dims as u64) as usize;
                v[bucket] += 1.0;
            }
            // L2-normalize (leave an all-zero vector — e.g. empty text — as-is
            // to avoid dividing by zero).
            let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                for x in v.iter_mut() {
                    *x /= norm;
                }
            }
            v
        }
    }

    impl Embed for FakeEmbedder {
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|t| self.embed_one(t)).collect())
        }

        fn dims(&self) -> usize {
            self.dims
        }
    }

    fn fake_engine(dir: &Path) -> Engine {
        fake_engine_result(dir).unwrap()
    }

    fn fake_engine_result(dir: &Path) -> Result<Engine> {
        Engine::open_with_embedder(dir, "fake-model", Box::new(FakeEmbedder { dims: 16 }))
    }

    #[test]
    fn second_open_while_first_held_is_engine_busy() {
        use crate::error::{is_engine_busy, EngineBusy};
        let dir = tempfile::tempdir().unwrap();
        // First handle takes redb's single-process lock and keeps it.
        let _held = fake_engine(dir.path());
        // A second open of the SAME cache dir must classify as EngineBusy,
        // not a generic error (redb is single-process by design). `Engine`
        // isn't `Debug`, so unwrap the error via `match` rather than
        // `expect_err`.
        let err = match Engine::open(dir.path(), "multilingual-e5-small") {
            Ok(_) => panic!("second open must fail while the first handle holds the lock"),
            Err(e) => e,
        };
        assert!(
            is_engine_busy(&err),
            "second open should be EngineBusy, got: {err:#}"
        );
        assert!(err.downcast_ref::<EngineBusy>().is_some());
    }

    /// #223: a holder that only touches the collection-level lock file
    /// (standing in for a legacy-unaware opener that would otherwise create
    /// a totally separate physical `engine.redb` at the legacy root, with no
    /// shared redb lock to catch the collision) must now block a normal
    /// split-aware `Engine::open` on the SAME collection root.
    #[test]
    fn legacy_holder_on_shared_root_lock_blocks_split_aware_open() {
        use crate::error::{is_engine_busy, EngineBusy};
        let dir = tempfile::tempdir().unwrap();
        let lock_path = CollectionLayout::new(dir.path()).lock_path();
        // Simulate a "legacy holder": takes the shared collection lock
        // directly, without going through `Engine::open` at all — this
        // stands in for the other side of the legacy/split boundary that
        // `open_inner`'s own migrate() cannot coordinate with (see #223).
        let _legacy_holder = Database::create(&lock_path).unwrap();

        let err = match fake_engine_result(dir.path()) {
            Ok(_) => panic!("open must fail while the legacy holder keeps the shared root lock"),
            Err(e) => e,
        };
        assert!(is_engine_busy(&err), "got: {err:#}");
        assert!(err.downcast_ref::<EngineBusy>().is_some());
    }

    /// Converse of the above: once a normal `Engine::open` holds the
    /// collection, a second, independent attempt to take the SAME
    /// collection-level lock (simulating a legacy-side actor arriving after
    /// a split-aware opener) must also fail — the lock is symmetric,
    /// regardless of which "side" gets there first.
    #[test]
    fn split_engine_blocks_a_second_holder_of_the_shared_root_lock() {
        let dir = tempfile::tempdir().unwrap();
        let _held = fake_engine(dir.path());

        let lock_path = CollectionLayout::new(dir.path()).lock_path();
        let result = Database::create(&lock_path);
        assert!(
            result.is_err(),
            "a second opener of the shared collection lock must be rejected while \
             the engine holds it"
        );
    }

    /// #223 regression (migrate-before-lock): the collection lock must be
    /// acquired BEFORE `migrate()` runs, because `migrate()` (the
    /// `fs::rename` of legacy artifacts into `index/`) is exactly the
    /// concurrent-unsafe I/O the lock exists to serialize. Here a holder
    /// keeps the shared lock while a POPULATED LEGACY collection sits on
    /// disk; a second open must be rejected by the lock and must NOT have
    /// migrated anything — no `index/` split dir may appear, and every
    /// legacy artifact must stay put. Under the buggy order (`migrate()`
    /// first, lock second) the open still returns `EngineBusy`, but only
    /// AFTER migrate() has already moved the artifacts — so the layout
    /// assertions below fail. Fake bytes suffice: with the lock held the
    /// open bails before it ever opens them as real stores.
    #[test]
    fn held_root_lock_prevents_unprotected_migration_of_legacy_collection() {
        use crate::error::is_engine_busy;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Populated legacy layout: artifacts at the collection root, no
        // split `index/` yet. (Names via a variable so the repo-wide "no
        // literal artifact joins" sweep stays clean.)
        let artifacts = ["tantivy", "vectors", "engine.redb"];
        std::fs::create_dir(root.join(artifacts[0])).unwrap();
        std::fs::write(root.join(artifacts[0]).join("seg"), b"lex").unwrap();
        std::fs::create_dir(root.join(artifacts[1])).unwrap();
        std::fs::write(root.join(artifacts[1]).join("v"), b"vec").unwrap();
        std::fs::write(root.join(artifacts[2]), b"redb").unwrap();
        assert_eq!(
            CollectionLayout::new(root).detect(),
            crate::layout::CacheLayoutState::Legacy
        );

        // Another opener holds the shared collection lock.
        let lock_path = CollectionLayout::new(root).lock_path();
        let _holder = Database::create(&lock_path).unwrap();

        // The second open must be rejected by the lock — and, crucially,
        // must not have run migrate() first.
        let err = match fake_engine_result(root) {
            Ok(_) => panic!("open must fail while the holder keeps the shared root lock"),
            Err(e) => e,
        };
        assert!(is_engine_busy(&err), "expected EngineBusy, got: {err:#}");

        // The load-bearing regression assertions: no unprotected migration
        // touched a collection another opener holds.
        assert!(
            !root.join("index").exists(),
            "migrate() must NOT run before the lock check — no split dir may \
             appear on a busy collection (#223 migrate-before-lock regression)"
        );
        for name in artifacts {
            assert!(
                root.join(name).exists(),
                "legacy {name} must stay at the collection root, unmigrated"
            );
        }
    }

    /// #223 regression (racy migrate): two processes racing `Engine::open`
    /// on the SAME still-legacy collection must never escape with a raw,
    /// unclassified I/O error — the loser either serializes cleanly behind
    /// the lock or gets a classified `EngineBusy`, and the winner completes
    /// the migration. Under the buggy migrate-before-lock order both threads
    /// can enter `migrate()` at once; one's `fs::rename` then hits ENOENT
    /// after the peer moved the source, and that ENOENT is NOT recognized by
    /// `classify_open_error`, so it surfaces as a raw error instead of the
    /// honest `EngineBusy`. Uses a REAL (dims-16 fake-embedder) index moved
    /// back to the legacy root so the winning open genuinely succeeds.
    #[test]
    fn concurrent_opens_of_populated_legacy_collection_never_escape_unclassified() {
        use crate::error::is_engine_busy;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let vault = tempfile::tempdir().unwrap();
        std::fs::write(vault.path().join("note.md"), "# hi\n\nsome body text").unwrap();

        // Build a real split index, then fake a legacy layout by moving each
        // artifact back to the collection root and dropping the split dir +
        // the setup engine's leftover lock, so the race starts clean.
        {
            let mut e = fake_engine(&root);
            e.skip_reranker_fetch_for_tests();
            e.reindex_all(vault.path()).unwrap();
        }
        let artifacts = ["tantivy", "vectors", "engine.redb"];
        for name in artifacts {
            let from = CollectionLayout::new(&root).index_artifact(name);
            if from != root.join(name) && from.exists() {
                std::fs::rename(&from, root.join(name)).unwrap();
            }
        }
        let _ = std::fs::remove_dir_all(root.join("index"));
        let _ = std::fs::remove_file(CollectionLayout::new(&root).lock_path());
        assert_eq!(
            CollectionLayout::new(&root).detect(),
            crate::layout::CacheLayoutState::Legacy
        );

        // Race two opens, released simultaneously by a barrier.
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let (root_a, root_b) = (root.clone(), root.clone());
        let (b_a, b_b) = (barrier.clone(), barrier.clone());
        let ta = std::thread::spawn(move || {
            b_a.wait();
            fake_engine_result(&root_a)
        });
        let tb = std::thread::spawn(move || {
            b_b.wait();
            fake_engine_result(&root_b)
        });
        let ra = ta.join().unwrap();
        let rb = tb.join().unwrap();

        // Neither thread may escape with a raw, unclassified error (e.g. a
        // migrate() rename ENOENT after the peer moved the source first).
        for (label, r) in [("A", &ra), ("B", &rb)] {
            if let Err(e) = r {
                assert!(
                    is_engine_busy(e),
                    "thread {label} escaped with an unclassified error — migrate() \
                     ran unprotected by the lock: {e:#}"
                );
            }
        }
        assert!(ra.is_ok() || rb.is_ok(), "at least one open must succeed");
        assert!(
            root.join("index").exists(),
            "the winning open must have completed the migration"
        );
    }

    /// An embedder whose every method panics. Used to prove a lex-only
    /// reindex never constructs/calls the embedder at all — if it did, the
    /// test process would abort on the panic rather than merely fail an
    /// assertion.
    struct PanicEmbed;

    impl Embed for PanicEmbed {
        fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>> {
            panic!("PanicEmbed::embed called — lex-only reindex must never embed");
        }
        fn dims(&self) -> usize {
            panic!("PanicEmbed::dims called — lex-only reindex must never construct/query the embedder");
        }
        fn embed_passages(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>> {
            panic!("PanicEmbed::embed_passages called — lex-only reindex must never embed");
        }
        fn embed_query(&self, _text: &str) -> Result<Vec<f32>> {
            panic!("PanicEmbed::embed_query called — lex-only reindex must never embed");
        }
    }

    /// Open an engine with [`PanicEmbed`] injected, at a fixed 16-dim vector
    /// store (matching `fake_engine`'s dims so the two are interchangeable in
    /// tests that switch embedders mid-test). `dims()` itself panics, so the
    /// vector store must be opened directly rather than via
    /// `Engine::open_with_embedder`, which calls `embedder.dims()`.
    fn panic_engine(dir: &Path) -> Engine {
        Engine::open_inner(
            dir,
            "panic-model",
            16,
            EmbedSource::Injected(Box::new(PanicEmbed)),
        )
        .unwrap()
    }

    #[test]
    fn fake_embedder_is_deterministic_and_distinguishes_texts() {
        let f = FakeEmbedder { dims: 16 };
        let a1 = f.embed(&["memory safety".to_string()]).unwrap();
        let a2 = f.embed(&["memory safety".to_string()]).unwrap();
        assert_eq!(a1, a2, "same text must embed identically");

        let b = f.embed(&["pasta recipe".to_string()]).unwrap();
        assert_ne!(a1[0], b[0], "distinct texts must embed differently");

        // L2-normalized.
        let norm: f32 = a1[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }

    #[test]
    fn fake_index_doc_then_query_returns_expected_top_hit() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(dir.path());
        e.index_doc("rust.md", "# Rust\nerror handling and memory safety")
            .unwrap();
        e.index_doc("cook.md", "# Cooking\npasta recipe with tomato")
            .unwrap();

        // Query with text drawn from the rust doc; it should top the fused
        // hybrid ranking (both lex and vector favour it).
        let hits = e.query("memory safety", 3).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].doc_path, "rust.md");
    }

    #[test]
    fn fake_vector_search_ranks_matching_doc_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(dir.path());
        e.index_doc("rust.md", "error handling and memory safety")
            .unwrap();
        e.index_doc("cook.md", "pasta recipe with tomato").unwrap();

        // Vector-only path: querying with a chunk's exact text yields a
        // cosine of 1.0 for that chunk, so it must rank first.
        let hits = e
            .vector_search("error handling and memory safety", 2)
            .unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].doc_path, "rust.md");
        // Exact-text match → cosine ~1.0.
        assert!(hits[0].score > 0.99, "score was {}", hits[0].score);
    }

    #[test]
    fn fake_remove_doc_excludes_it_from_query() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(dir.path());
        e.index_doc("rust.md", "error handling and memory safety")
            .unwrap();
        e.index_doc("cook.md", "pasta recipe with tomato").unwrap();
        assert!(e.get("rust.md").is_ok());

        e.remove_doc("rust.md").unwrap();
        assert!(e.get("rust.md").is_err(), "get must fail after removal");

        let hits = e.query("memory safety", 5).unwrap();
        assert!(
            hits.iter().all(|h| h.doc_path != "rust.md"),
            "removed doc must not appear in results"
        );
    }

    #[test]
    fn fake_get_concatenates_chunks_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(dir.path());
        e.index_doc("a.md", "# Heading\nalpha beta gamma").unwrap();
        let text = e.get("a.md").unwrap();
        assert!(text.contains("alpha beta gamma"));
    }

    #[test]
    fn fake_reindex_detects_add_update_unchanged_remove() {
        let cache_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(cache_dir.path());

        let doc_path = vault_dir.path().join("a.md");
        std::fs::write(&doc_path, "# A\noriginal content").unwrap();

        // Added.
        let stats = e.reindex_all(vault_dir.path()).unwrap();
        assert_eq!(
            stats,
            ReindexStats {
                added: 1,
                updated: 0,
                removed: 0,
                unchanged: 0,
                failed: 0,
            }
        );

        // Unchanged (no edit).
        let stats = e.reindex_all(vault_dir.path()).unwrap();
        assert_eq!(
            stats,
            ReindexStats {
                added: 0,
                updated: 0,
                removed: 0,
                unchanged: 1,
                failed: 0,
            }
        );

        // Updated (bytes differ).
        std::fs::write(&doc_path, "# A\nedited content, different bytes").unwrap();
        let stats = e.reindex_all(vault_dir.path()).unwrap();
        assert_eq!(
            stats,
            ReindexStats {
                added: 0,
                updated: 1,
                removed: 0,
                unchanged: 0,
                failed: 0,
            }
        );
        assert!(e.get("a.md").is_ok());

        // Removed (file deleted).
        std::fs::remove_file(&doc_path).unwrap();
        let stats = e.reindex_all(vault_dir.path()).unwrap();
        assert_eq!(
            stats,
            ReindexStats {
                added: 0,
                updated: 0,
                removed: 1,
                unchanged: 0,
                failed: 0,
            }
        );
        assert!(e.get("a.md").is_err());
    }

    #[test]
    fn fake_status_reports_doc_count_last_indexed_and_no_drift_after_reindex() {
        let cache_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(cache_dir.path());

        // Fresh index: nothing stored, so status walks the (empty) vault and
        // reports a clean, never-indexed state.
        let fresh = e.status(vault_dir.path()).unwrap();
        assert_eq!(fresh.doc_count, 0);
        assert_eq!(fresh.last_indexed_at, None);
        assert_eq!(fresh.pending_total(), 0);

        std::fs::write(vault_dir.path().join("a.md"), "# A\nalpha content").unwrap();
        std::fs::write(vault_dir.path().join("b.md"), "# B\nbeta content").unwrap();

        // Before reindex, both files are pending-new drift and there's still
        // no last_indexed timestamp.
        let before = e.status(vault_dir.path()).unwrap();
        assert_eq!(before.doc_count, 0);
        assert_eq!(before.pending_new, 2);
        assert_eq!(before.pending_changed, 0);
        assert_eq!(before.pending_removed, 0);
        assert_eq!(before.last_indexed_at, None);

        e.reindex_all(vault_dir.path()).unwrap();

        // After reindex: both docs indexed, timestamp set, no drift.
        let after = e.status(vault_dir.path()).unwrap();
        assert_eq!(after.doc_count, 2);
        assert!(after.last_indexed_at.is_some());
        assert_eq!(after.pending_total(), 0);
    }

    #[test]
    fn fake_status_detects_changed_and_removed_drift() {
        let cache_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(cache_dir.path());

        let a = vault_dir.path().join("a.md");
        let b = vault_dir.path().join("b.md");
        std::fs::write(&a, "# A\nalpha content").unwrap();
        std::fs::write(&b, "# B\nbeta content").unwrap();
        e.reindex_all(vault_dir.path()).unwrap();
        assert_eq!(e.status(vault_dir.path()).unwrap().pending_total(), 0);

        // Edit a, delete b, add c → 1 changed, 1 removed, 1 new.
        std::fs::write(&a, "# A\nalpha content EDITED").unwrap();
        std::fs::remove_file(&b).unwrap();
        std::fs::write(vault_dir.path().join("c.md"), "# C\ngamma").unwrap();

        let s = e.status(vault_dir.path()).unwrap();
        assert_eq!(s.doc_count, 2, "still 2 indexed until next reindex");
        assert_eq!(s.pending_new, 1);
        assert_eq!(s.pending_changed, 1);
        assert_eq!(s.pending_removed, 1);
        assert_eq!(s.pending_total(), 3);
    }

    #[test]
    fn fake_reindex_paths_sets_last_indexed() {
        let cache_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(cache_dir.path());
        std::fs::write(vault_dir.path().join("a.md"), "# A\nalpha").unwrap();

        assert_eq!(e.status(vault_dir.path()).unwrap().last_indexed_at, None);
        e.reindex_paths(vault_dir.path(), &["a.md".to_string()])
            .unwrap();
        assert!(e
            .status(vault_dir.path())
            .unwrap()
            .last_indexed_at
            .is_some());
    }

    #[test]
    fn index_status_pending_total_sums_categories() {
        let s = IndexStatus {
            doc_count: 10,
            last_indexed_at: Some(123),
            pending_new: 2,
            pending_changed: 3,
            pending_removed: 1,
        };
        assert_eq!(s.pending_total(), 6);
    }

    #[test]
    fn fake_reindex_all_with_progress_reports_increasing_done_and_model_load() {
        // Drives the progress-aware reindex with the fake embedder (no model
        // download) and asserts the callback fired: `LoadingModel` once before
        // the first embed, then `Indexing` with `done` climbing 1..=total.
        let cache_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(cache_dir.path());

        std::fs::write(vault_dir.path().join("a.md"), "# A\nalpha content").unwrap();
        std::fs::write(vault_dir.path().join("b.md"), "# B\nbeta content").unwrap();
        std::fs::write(vault_dir.path().join("c.md"), "# C\ngamma content").unwrap();

        let mut events = Vec::new();
        let stats = e
            .reindex_all_with_progress(vault_dir.path(), &mut |p| events.push(p))
            .unwrap();
        assert_eq!(stats.added, 3);

        // Walked fires first, carrying the run total, before anything else.
        assert_eq!(
            events.first(),
            Some(&ReindexProgress::Walked { total: 3 }),
            "Walked {{ total }} must be the first event"
        );

        // Exactly one LoadingModel, emitted before any Indexing event.
        let loading = events
            .iter()
            .filter(|p| matches!(p, ReindexProgress::LoadingModel))
            .count();
        assert_eq!(loading, 1, "LoadingModel must fire exactly once");
        let first_loading = events
            .iter()
            .position(|p| matches!(p, ReindexProgress::LoadingModel))
            .unwrap();
        let first_indexing = events
            .iter()
            .position(|p| matches!(p, ReindexProgress::Indexing { .. }))
            .unwrap();
        assert!(
            first_loading < first_indexing,
            "LoadingModel must precede the first Indexing event"
        );

        // Indexing events count 1..=total with a stable total.
        let indexing: Vec<(usize, usize)> = events
            .iter()
            .filter_map(|p| match p {
                ReindexProgress::Indexing { done, total, .. } => Some((*done, *total)),
                _ => None,
            })
            .collect();
        assert_eq!(indexing.len(), 3);
        assert!(indexing.iter().all(|(_, total)| *total == 3));
        let dones: Vec<usize> = indexing.iter().map(|(done, _)| *done).collect();
        assert_eq!(dones, vec![1, 2, 3], "done must climb 1..=total in order");

        // A second run with no edits is all-unchanged: no embed, so no
        // LoadingModel, but every doc still emits an Indexing event.
        let mut events2 = Vec::new();
        let stats2 = e
            .reindex_all_with_progress(vault_dir.path(), &mut |p| events2.push(p))
            .unwrap();
        assert_eq!(stats2.unchanged, 3);
        assert!(
            !events2
                .iter()
                .any(|p| matches!(p, ReindexProgress::LoadingModel)),
            "all-unchanged run must not announce a model load"
        );
        assert_eq!(
            events2
                .iter()
                .filter(|p| matches!(p, ReindexProgress::Indexing { .. }))
                .count(),
            3
        );
    }

    #[test]
    fn should_fetch_reranker_truth_table() {
        assert!(
            should_fetch_reranker(true, false),
            "enabled + not downloaded => fetch"
        );
        assert!(
            !should_fetch_reranker(true, true),
            "enabled + already downloaded => no fetch"
        );
        assert!(
            !should_fetch_reranker(false, false),
            "disabled + not downloaded => no fetch"
        );
        assert!(
            !should_fetch_reranker(false, true),
            "disabled + already downloaded => no fetch"
        );
    }

    #[test]
    fn fake_reindex_all_with_progress_emits_loading_reranker_after_indexing() {
        // Default rerank settings are enabled, and a fresh temp cache_dir has
        // no reranker model downloaded, so `should_fetch_reranker` is true and
        // the reindex EMITS `LoadingReranker`. This test asserts only that
        // event's presence + ordering — it must NOT actually fetch the model,
        // so we suppress the ~570 MB hf-hub download via the test seam. (The
        // real download is covered by the network-gated `ONEBRAIN_TEST_RERANK`
        // test below.) Assert LoadingReranker fires exactly once, after every
        // Indexing event.
        let cache_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(cache_dir.path());
        e.skip_reranker_fetch_for_tests();

        std::fs::write(vault_dir.path().join("a.md"), "# A\nalpha content").unwrap();
        std::fs::write(vault_dir.path().join("b.md"), "# B\nbeta content").unwrap();

        let mut events = Vec::new();
        e.reindex_all_with_progress(vault_dir.path(), &mut |p| events.push(p))
            .unwrap();

        let loading_reranker_count = events
            .iter()
            .filter(|p| matches!(p, ReindexProgress::LoadingReranker))
            .count();
        assert_eq!(
            loading_reranker_count, 1,
            "LoadingReranker must fire exactly once: {events:?}"
        );
        let last_indexing = events
            .iter()
            .rposition(|p| matches!(p, ReindexProgress::Indexing { .. }))
            .unwrap();
        let loading_reranker_pos = events
            .iter()
            .position(|p| matches!(p, ReindexProgress::LoadingReranker))
            .unwrap();
        assert!(
            loading_reranker_pos > last_indexing,
            "LoadingReranker must come after every Indexing event: {events:?}"
        );
    }

    #[test]
    fn fake_reindex_all_with_progress_skips_loading_reranker_when_disabled() {
        let cache_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(cache_dir.path());
        e.set_rerank_settings(RerankSettings {
            enabled: false,
            ..Default::default()
        });

        std::fs::write(vault_dir.path().join("a.md"), "# A\nalpha content").unwrap();

        let mut events = Vec::new();
        e.reindex_all_with_progress(vault_dir.path(), &mut |p| events.push(p))
            .unwrap();

        assert!(
            !events
                .iter()
                .any(|p| matches!(p, ReindexProgress::LoadingReranker)),
            "disabled reranker must never emit LoadingReranker: {events:?}"
        );
    }

    // Live network test: a full reindex with the reranker enabled and its
    // model NOT yet on disk must actually DOWNLOAD it (not just emit the
    // event). Gated behind ONEBRAIN_TEST_RERANK because it fetches ~570MB
    // from Hugging Face. This is the test the unit suite structurally could
    // not have: it exercises the real hf-hub path, and it fails on the
    // circular-skip deadlock (reindex→reranker()→build_reranker skips) that
    // shipped in the first v3.4.7 cut and was caught only on the real vault.
    #[cfg(feature = "semantic")]
    #[test]
    fn reindex_downloads_reranker_model_when_missing() {
        if std::env::var("ONEBRAIN_TEST_RERANK").is_err() {
            return; // gated: multi-hundred-MB network download
        }
        let cache_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        // Fake embedder (so embedding needs no model), real reranker source
        // (default settings: enabled + onebrain-rerank-v1). The reranker
        // fetch path is independent of the injected embedder.
        let mut e = fake_engine(cache_dir.path());
        std::fs::write(vault_dir.path().join("a.md"), "# A\nalpha content").unwrap();

        let info = rerank::reranker_registry()
            .iter()
            .find(|m| m.name == "onebrain-rerank-v1")
            .unwrap();
        assert!(
            !rerank::reranker_download_status(info, cache_dir.path()).downloaded,
            "precondition: model must be absent before reindex"
        );

        e.reindex_all_with_progress(vault_dir.path(), &mut |_| {})
            .unwrap();

        assert!(
            rerank::reranker_download_status(info, cache_dir.path()).downloaded,
            "reindex must leave the reranker model on disk — a query-path skip \
             would silently never download it"
        );
    }

    #[test]
    fn fake_rebuild_with_embedder_reports_batched_progress() {
        // Index a few docs with the fake embedder, then rebuild and assert
        // the progress callback fired (0, total) first and (total, total)
        // last, with done strictly increasing.
        let cache_dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(cache_dir.path());
        e.index_doc("a.md", "# A\nalpha content").unwrap();
        e.index_doc("b.md", "# B\nbeta content").unwrap();

        let mut events: Vec<(usize, usize)> = Vec::new();
        e.rebuild_with_embedder(
            "fake-model-2",
            Box::new(FakeEmbedder { dims: 16 }),
            &mut |done, total| events.push((done, total)),
        )
        .unwrap();

        assert!(events.len() >= 2, "at least (0,total) and (total,total)");
        let total = events[0].1;
        assert!(total >= 2, "two docs produce at least two chunks");
        assert_eq!(events.first(), Some(&(0, total)));
        assert_eq!(events.last(), Some(&(total, total)));
        assert!(
            events.windows(2).all(|w| w[0].0 < w[1].0),
            "done strictly increases: {events:?}"
        );
        assert!(events.iter().all(|(_, t)| *t == total), "stable total");
    }

    #[test]
    fn fake_reindex_paths_with_progress_reports_events() {
        let cache_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(cache_dir.path());
        std::fs::write(vault_dir.path().join("a.md"), "# A\nalpha content").unwrap();

        let mut events = Vec::new();
        let stats = e
            .reindex_paths_with_progress(vault_dir.path(), &["a.md".to_string()], &mut |p| {
                events.push(p)
            })
            .unwrap();
        assert_eq!(stats.added, 1);
        assert_eq!(
            events.first(),
            Some(&ReindexProgress::Walked { total: 1 }),
            "Walked {{ total }} must be the first event"
        );
        assert!(events
            .iter()
            .any(|p| matches!(p, ReindexProgress::LoadingModel)));
        assert_eq!(
            events.last(),
            Some(&ReindexProgress::Indexing {
                done: 1,
                total: 1,
                doc_path: "a.md".to_string(),
            })
        );
    }

    #[test]
    fn fake_reindex_paths_targets_specific_docs() {
        let cache_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(cache_dir.path());

        let a = vault_dir.path().join("a.md");
        std::fs::write(&a, "# A\nalpha content").unwrap();

        // Add via targeted path.
        let stats = e
            .reindex_paths(vault_dir.path(), &["a.md".to_string()])
            .unwrap();
        assert_eq!(stats.added, 1);
        assert!(e.get("a.md").is_ok());

        // A path that is neither on disk nor indexed is ignored (no-op).
        let stats = e
            .reindex_paths(vault_dir.path(), &["ghost.md".to_string()])
            .unwrap();
        assert_eq!(stats, ReindexStats::default());

        // Delete on disk, then a targeted reindex removes it.
        std::fs::remove_file(&a).unwrap();
        let stats = e
            .reindex_paths(vault_dir.path(), &["a.md".to_string()])
            .unwrap();
        assert_eq!(stats.removed, 1);
        assert!(e.get("a.md").is_err());
    }

    #[test]
    fn fake_rebuild_to_different_dims_reembeds_and_query_works() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(dir.path()); // dims 16
        e.index_doc("rust.md", "error handling and memory safety")
            .unwrap();
        e.index_doc("cook.md", "pasta recipe with tomato").unwrap();
        assert!(e.active_model_matches("fake-model").unwrap());

        // Rebuild to a different-dims fake embedder: vector store is dropped
        // and re-created at 32 dims, every chunk re-embedded.
        let reembedded = e
            .rebuild_with_embedder(
                "fake-model-2",
                Box::new(FakeEmbedder { dims: 32 }),
                &mut |_, _| {},
            )
            .unwrap();
        assert_eq!(reembedded, 2);
        assert!(e.active_model_matches("fake-model-2").unwrap());
        assert!(!e.active_model_matches("fake-model").unwrap());

        // Query still works against the re-embedded (32-dim) store.
        let hits = e.query("memory safety", 3).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].doc_path, "rust.md");
    }

    #[test]
    fn fake_rebuild_empty_index_reembeds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(dir.path());
        // No docs indexed → rebuild re-embeds 0 chunks.
        let reembedded = e
            .rebuild_with_embedder(
                "fake-model-2",
                Box::new(FakeEmbedder { dims: 8 }),
                &mut |_, _| {},
            )
            .unwrap();
        assert_eq!(reembedded, 0);
        assert!(e.active_model_matches("fake-model-2").unwrap());
    }

    #[test]
    fn index_then_query_roundtrip() {
        if std::env::var("ONEBRAIN_TEST_EMBED").is_err() {
            return; // gated: downloads a model
        }
        let dir = tempfile::tempdir().unwrap();
        let mut e = Engine::open(dir.path(), "multilingual-e5-small").unwrap();
        e.index_doc("rust.md", "# Rust\nerror handling and memory safety")
            .unwrap();
        e.index_doc("cook.md", "# Cooking\npasta recipe with tomato")
            .unwrap();
        let hits = e.query("memory safety", 3).unwrap();
        assert_eq!(hits[0].doc_path, "rust.md");
    }

    #[test]
    fn remove_doc_clears_meta() {
        if std::env::var("ONEBRAIN_TEST_EMBED").is_err() {
            return; // gated: downloads a model
        }
        let dir = tempfile::tempdir().unwrap();
        let mut e = Engine::open(dir.path(), "multilingual-e5-small").unwrap();
        e.index_doc("rust.md", "# Rust\nerror handling and memory safety")
            .unwrap();
        assert!(e.get("rust.md").is_ok());
        e.remove_doc("rust.md").unwrap();
        assert!(e.get("rust.md").is_err());
        let hits = e.query("memory safety", 3).unwrap();
        assert!(hits.iter().all(|h| h.doc_path != "rust.md"));
    }

    #[test]
    fn reindex_detects_add_update_unchanged_remove() {
        if std::env::var("ONEBRAIN_TEST_EMBED").is_err() {
            return; // gated: downloads a model
        }
        let cache_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let mut e = Engine::open(cache_dir.path(), "multilingual-e5-small").unwrap();

        let doc_path = vault_dir.path().join("a.md");
        std::fs::write(&doc_path, "# A\noriginal content").unwrap();

        let stats = e.reindex_all(vault_dir.path()).unwrap();
        assert_eq!(
            stats,
            ReindexStats {
                added: 1,
                updated: 0,
                removed: 0,
                unchanged: 0,
                failed: 0,
            }
        );

        // Reindexing again with no changes -> unchanged.
        let stats = e.reindex_all(vault_dir.path()).unwrap();
        assert_eq!(
            stats,
            ReindexStats {
                added: 0,
                updated: 0,
                removed: 0,
                unchanged: 1,
                failed: 0,
            }
        );

        // Edit the file -> updated.
        std::fs::write(&doc_path, "# A\nedited content, different bytes").unwrap();
        let stats = e.reindex_all(vault_dir.path()).unwrap();
        assert_eq!(
            stats,
            ReindexStats {
                added: 0,
                updated: 1,
                removed: 0,
                unchanged: 0,
                failed: 0,
            }
        );
        assert!(e.get("a.md").is_ok());

        // Delete the file -> removed.
        std::fs::remove_file(&doc_path).unwrap();
        let stats = e.reindex_all(vault_dir.path()).unwrap();
        assert_eq!(
            stats,
            ReindexStats {
                added: 0,
                updated: 0,
                removed: 1,
                unchanged: 0,
                failed: 0,
            }
        );
        assert!(e.get("a.md").is_err());
    }

    #[test]
    fn multilingual_semantic_search() {
        if std::env::var("ONEBRAIN_TEST_EMBED").is_err() {
            return; // gated: downloads a model
        }
        let dir = tempfile::tempdir().unwrap();
        let mut e = Engine::open(dir.path(), "multilingual-e5-small").unwrap();
        e.index_doc(
            "en.md",
            "# Machine learning\nneural networks and model training",
        )
        .unwrap();
        e.index_doc("th.md", "# การทำอาหาร\nสูตรผัดไทยและส่วนผสม")
            .unwrap(); // Thai: cooking / pad thai recipe
        e.index_doc("zh.md", "# 天气\n今天下雨很冷需要带伞")
            .unwrap(); // Chinese: weather / rain
                       // A Chinese query about weather should rank the Chinese weather doc
                       // first, over the English ML doc and Thai cooking doc — proving
                       // cross-doc multilingual semantics.
        let hits = e.query("下雨天气", 3).unwrap();
        assert_eq!(hits[0].doc_path, "zh.md");
    }

    #[test]
    fn reopen_after_lex_schema_change_repopulates_from_meta() {
        // Upgrade path: a vault indexed by an older build has a tantivy index
        // whose schema no longer matches. Reopening must self-heal — wipe the
        // lex index and refill it from redb's chunk_meta — WITHOUT re-reading
        // vault files or re-embedding. Simulated by replacing the tantivy dir
        // with a foreign-schema index between two opens.
        let dir = tempfile::tempdir().unwrap();
        let tantivy_dir = {
            let mut e = fake_engine(dir.path());
            e.index_doc("rust.md", "# Rust\nerror handling and memory safety")
                .unwrap();
            e.index_doc("cook.md", "# Cooking\npasta recipe with tomato")
                .unwrap();
            assert!(!e.lex.search("error", 5).unwrap().is_empty());
            e.layout.index_artifact("tantivy")
        };

        // Replace with an index carrying a DIFFERENT schema.
        std::fs::remove_dir_all(&tantivy_dir).unwrap();
        std::fs::create_dir_all(&tantivy_dir).unwrap();
        {
            let mut sb = tantivy::schema::Schema::builder();
            sb.add_text_field("something_else", tantivy::schema::TEXT);
            tantivy::Index::builder()
                .schema(sb.build())
                .open_or_create(tantivy::directory::MmapDirectory::open(&tantivy_dir).unwrap())
                .unwrap();
        }

        // Reopening succeeds and the lex index is whole again.
        let e = fake_engine(dir.path());
        let hits = e.lex.search("error", 5).unwrap();
        assert_eq!(
            hits.len(),
            1,
            "lex index must be repopulated from chunk_meta after a schema reset"
        );
        assert!(hits[0].0.starts_with("rust.md#"));
        assert!(!e.lex.search("pasta", 5).unwrap().is_empty());
        // The doc's stored text is untouched — nothing was re-read or re-embedded.
        assert!(e.get("rust.md").unwrap().contains("memory safety"));
        // A completed rebuild clears its own marker, so the next open is a
        // plain open.
        assert!(!lex::rebuild_pending(&tantivy_dir));
        assert!(e.lex_health().unwrap().is_healthy());
    }

    /// Reproduce the on-disk state a migration interrupted between the wipe
    /// and the repopulate's commit leaves behind: the tantivy dir wiped and
    /// recreated EMPTY under the CURRENT schema (so the next open reports no
    /// mismatch and no reset), while redb still holds every chunk. Returns the
    /// tantivy dir.
    ///
    /// Faithful to the real crash: it goes through `LexIndex::open_or_reset`
    /// itself — including the marker write — and then simply never
    /// repopulates, which is precisely what a Ctrl-C during the rebuild does.
    fn simulate_interrupted_lex_migration(cache_dir: &Path) -> PathBuf {
        let tantivy_dir = CollectionLayout::new(cache_dir).index_artifact("tantivy");
        // Foreign schema ⇒ `open_or_reset` takes the wipe branch, exactly as
        // an upgraded vault does.
        std::fs::remove_dir_all(&tantivy_dir).unwrap();
        std::fs::create_dir_all(&tantivy_dir).unwrap();
        {
            let mut sb = tantivy::schema::Schema::builder();
            sb.add_text_field("something_else", tantivy::schema::TEXT);
            tantivy::Index::builder()
                .schema(sb.build())
                .open_or_create(tantivy::directory::MmapDirectory::open(&tantivy_dir).unwrap())
                .unwrap();
        }
        let (_lex, was_reset) = LexIndex::open_or_reset(&tantivy_dir).unwrap();
        assert!(was_reset);
        // ...and here the process dies. No repopulate, no marker clear.
        tantivy_dir
    }

    #[test]
    fn interrupted_lex_migration_is_healed_on_the_next_open() {
        // B1 (BLOCKER): the wipe is recorded on disk, not just in memory. The
        // post-crash directory has a MATCHING schema, so `open_or_reset`
        // reports `was_reset == false` — if that flag were the only trigger,
        // the lex index would stay empty forever: queries return 0 hits with
        // exit 0, `status` reports every doc indexed, and `reindex` (even a
        // full one) skips every doc because `lex_hashes` says it is current.
        // Only `reindex --force` would recover it. The marker is what makes
        // the next plain open heal it.
        let dir = tempfile::tempdir().unwrap();
        {
            let mut e = fake_engine(dir.path());
            e.index_doc("rust.md", "# Rust\nerror handling and memory safety")
                .unwrap();
            e.index_doc("cook.md", "# Cooking\npasta recipe with tomato")
                .unwrap();
        }

        let tantivy_dir = simulate_interrupted_lex_migration(dir.path());
        assert!(
            lex::rebuild_pending(&tantivy_dir),
            "the interrupted migration must have left its marker behind"
        );
        // Confirm the trap is real: nothing else on disk says anything is
        // wrong — the schema now matches, so a reopen sees no reset at all.
        {
            let (probe, was_reset) = LexIndex::open_or_reset(&tantivy_dir).unwrap();
            assert!(!was_reset, "post-crash schema matches — no reset reported");
            assert_eq!(probe.num_docs().unwrap(), 0, "and the index is empty");
        }

        // The next ordinary open heals it, with no --force and no reindex.
        let e = fake_engine(dir.path());
        assert!(
            !e.lex.search("error", 5).unwrap().is_empty(),
            "an interrupted migration must be repaired by the next open"
        );
        assert!(!e.lex.search("pasta", 5).unwrap().is_empty());
        assert!(
            !lex::rebuild_pending(&tantivy_dir),
            "a completed rebuild must clear its marker"
        );
        let health = e.lex_health().unwrap();
        assert!(health.is_healthy(), "unexpectedly unhealthy: {health:?}");
        assert_eq!(health.lex_docs, health.chunk_meta);
    }

    /// Plant a rebuild marker WITHOUT touching the index — the state a failed
    /// marker-clear leaves behind (the clear warns and carries on, by design):
    /// pending marker over a fully populated, current-schema index.
    fn plant_rebuild_marker(tantivy_dir: &Path) {
        std::fs::write(lex::rebuild_marker_path(tantivy_dir), "onebrain: test\n").unwrap();
        assert!(lex::rebuild_pending(tantivy_dir));
    }

    #[test]
    fn a_marker_rebuild_over_a_populated_index_does_not_duplicate_it() {
        // B-A1 (BLOCKER): the rebuild is marker-driven, but the repopulate
        // used to assume it ran over an EMPTY index (`LexIndex::add` never
        // replaces). When the MARKER ALONE triggers it the schema still
        // matches, so `open_or_reset` wipes nothing and returns the fully
        // populated index — and the rebuild was appended on top, doubling
        // every chunk. Nothing noticed: `lex_health` only flagged an EMPTY
        // index, so doctor reported "healthy" over a doubled index whose BM25
        // statistics (document frequency, average field length) are corrupt.
        let dir = tempfile::tempdir().unwrap();
        let (tantivy_dir, before) = {
            let mut e = fake_engine(dir.path());
            e.index_doc("rust.md", "# Rust\nerror handling and memory safety")
                .unwrap();
            e.index_doc("cook.md", "# Cooking\npasta recipe with tomato")
                .unwrap();
            let h = e.lex_health().unwrap();
            assert!(h.is_healthy() && h.lex_docs > 0);
            (e.layout.index_artifact("tantivy"), h)
        };

        // Precondition of the bug: marker present, index NOT empty, schema
        // current (so `open_or_reset` reports no reset and wipes nothing).
        plant_rebuild_marker(&tantivy_dir);
        {
            let (probe, was_reset) = LexIndex::open_or_reset(&tantivy_dir).unwrap();
            assert!(!was_reset, "schema matches — no wipe happens on this path");
            assert_eq!(
                probe.num_docs().unwrap(),
                before.lex_docs,
                "the index must still be POPULATED when the rebuild is triggered"
            );
        }

        let e = fake_engine(dir.path());
        let after = e.lex_health().unwrap();
        assert_eq!(
            after.lex_docs, after.chunk_meta,
            "a marker-triggered rebuild must REPLACE, not append: {after:?}"
        );
        assert_eq!(after.lex_docs, before.lex_docs, "{after:?}");
        assert!(after.is_healthy(), "{after:?}");
        assert!(!lex::rebuild_pending(&tantivy_dir));
        // Still searchable, exactly once per chunk.
        assert_eq!(e.lex.search("error", 10).unwrap().len(), 1);
        assert_eq!(e.lex.search("pasta", 10).unwrap().len(), 1);
    }

    #[test]
    fn repeated_opens_with_a_surviving_marker_do_not_grow_the_index() {
        // The compounding form of B-A1, and the reason it is not self-
        // limiting: when the post-rebuild marker clear FAILS (read-only
        // `index/`) the open warns and carries on by design, so the marker
        // survives and EVERY subsequent open rebuilds again — observed as
        // 6271 → 43897 docs over seven opens on the real binary, with hit
        // counts decaying 5 → 4 → 3 as the duplicate documents wrecked BM25's
        // corpus statistics. The count must now be stable instead.
        let dir = tempfile::tempdir().unwrap();
        let tantivy_dir = {
            let mut e = fake_engine(dir.path());
            e.index_doc("rust.md", "# Rust\nerror handling and memory safety")
                .unwrap();
            e.index_doc("cook.md", "# Cooking\npasta recipe with tomato")
                .unwrap();
            e.layout.index_artifact("tantivy")
        };
        // An unremovable marker (a non-empty DIRECTORY at the marker path)
        // reproduces the surviving-marker path portably: `clear_rebuild_marker`
        // fails on every open, so every open re-runs the rebuild.
        std::fs::create_dir_all(lex::rebuild_marker_path(&tantivy_dir).join("occupied")).unwrap();
        assert!(lex::clear_rebuild_marker(&tantivy_dir).is_err());

        let mut counts = Vec::new();
        for _ in 0..3 {
            let e = fake_engine(dir.path());
            let h = e.lex_health().unwrap();
            assert!(
                h.rebuild_pending,
                "the marker must survive, or this test proves nothing: {h:?}"
            );
            assert_eq!(h.lex_docs, h.chunk_meta, "{h:?}");
            assert_eq!(e.lex.search("error", 10).unwrap().len(), 1, "{h:?}");
            counts.push(h.lex_docs);
        }
        assert!(
            counts.windows(2).all(|w| w[0] == w[1]),
            "repeated rebuilds must not grow the index: {counts:?}"
        );
    }

    #[test]
    fn lex_health_flags_an_index_holding_more_docs_than_chunks() {
        // Audit concern 1: `chunk_meta 6271 / lex_docs 12542` used to report
        // as "healthy" because only an EMPTY index counted as broken. Skew
        // UPWARD has no benign explanation (see `has_excess_docs`).
        let over = LexHealth {
            lex_docs: 12542,
            chunk_meta: 6271,
            rebuild_pending: false,
        };
        assert!(over.has_excess_docs());
        assert!(!over.is_dead(), "not dead — search still answers, worse");
        assert!(!over.is_healthy(), "a doubled index is not healthy");

        // Matching counts, and the benign DOWNWARD skew (a corrupt chunk
        // skipped by the rebuild), stay healthy — no crying wolf.
        for (lex_docs, chunk_meta) in [(6271, 6271), (6270, 6271), (0, 0)] {
            let h = LexHealth {
                lex_docs,
                chunk_meta,
                rebuild_pending: false,
            };
            assert!(!h.has_excess_docs(), "{h:?}");
            assert!(h.is_healthy(), "{h:?}");
        }
    }

    #[test]
    fn repopulate_replaces_an_over_populated_index() {
        // The repair half: whatever produced the surplus (duplicates from an
        // old build, or orphan lex docs left by a crash between `remove_doc`'s
        // redb commit and its lex commit), one repopulate restores
        // `lex_docs == chunk_meta`. This is what makes `doctor --fix` able to
        // repair the state above.
        let dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(dir.path());
        e.index_doc("rust.md", "# Rust\nerror handling and memory safety")
            .unwrap();
        let baseline = e.lex_health().unwrap();

        // Duplicate every chunk, plus an orphan with no chunk_meta row.
        for id in 0..2 {
            e.lex
                .add(&Chunk {
                    chunk_id: format!("ghost.md#{id}"),
                    doc_path: "ghost.md".to_string(),
                    heading_path: "Ghost".to_string(),
                    chunk_index: id,
                    text: "error handling and memory safety".to_string(),
                })
                .unwrap();
        }
        e.lex.commit().unwrap();
        let broken = e.lex_health().unwrap();
        assert!(broken.has_excess_docs(), "{broken:?}");

        let restored = e.repopulate_lex_from_meta().unwrap();
        let after = e.lex_health().unwrap();
        assert_eq!(after.lex_docs, after.chunk_meta, "{after:?}");
        assert_eq!(after.lex_docs as usize, restored, "{after:?}");
        assert_eq!(after.lex_docs, baseline.lex_docs, "{after:?}");
        assert!(after.is_healthy(), "{after:?}");
        assert!(
            e.lex.search("ghost", 10).unwrap().is_empty(),
            "the orphan documents must be gone"
        );
        assert_eq!(e.lex.search("error", 10).unwrap().len(), 1);
    }

    #[test]
    fn a_post_commit_marker_clear_failure_does_not_fail_the_open() {
        // B-R1 companion: the rebuild's durable part is the lex COMMIT. If
        // removing the bookkeeping marker afterwards fails (read-only
        // `index/`, exotic FS), propagating that error aborted `Engine::open`
        // — and the next open would see matching schema + pending marker →
        // same rebuild → same failure, i.e. a collection permanently
        // unopenable through a purely cosmetic problem. It must warn and
        // carry on instead.
        //
        // The unremovable marker is a non-empty DIRECTORY at the marker path:
        // `remove_file` on it fails with something that is emphatically not
        // `NotFound` (the one kind already treated as success), portably, with
        // no chmod/root dependency.
        let dir = tempfile::tempdir().unwrap();
        {
            let mut e = fake_engine(dir.path());
            e.index_doc("rust.md", "# Rust\nerror handling and memory safety")
                .unwrap();
        }
        let tantivy_dir = simulate_interrupted_lex_migration(dir.path());
        let marker = lex::rebuild_marker_path(&tantivy_dir);
        std::fs::remove_file(&marker).unwrap();
        std::fs::create_dir_all(marker.join("occupied")).unwrap();
        assert!(
            lex::rebuild_pending(&tantivy_dir),
            "the unremovable stand-in must still read as a pending rebuild"
        );
        // Prove the clear really cannot succeed — otherwise this test would
        // pass for the wrong reason.
        assert!(
            lex::clear_rebuild_marker(&tantivy_dir).is_err(),
            "the marker must be genuinely unremovable for this test to mean anything"
        );

        // The open must still succeed AND the rebuild must still have run.
        let e = fake_engine(dir.path());
        assert!(
            !e.lex.search("error", 5).unwrap().is_empty(),
            "the rebuild itself must have committed despite the clear failing"
        );
        assert_eq!(e.lex_health().unwrap().lex_docs, 1);
    }

    #[test]
    fn lex_health_reports_the_interrupted_migration_state() {
        // The CLI-facing probe (doctor / status) must be able to SEE the dead
        // state, not just repair it — a user whose index died before this fix
        // shipped needs to be told, and a rebuild that keeps failing must not
        // stay invisible. Checked against a live half-migrated collection: the
        // lex index is empty while chunk_meta is full.
        let dir = tempfile::tempdir().unwrap();
        {
            let mut e = fake_engine(dir.path());
            e.index_doc("rust.md", "# Rust\nerror handling and memory safety")
                .unwrap();
            let healthy = e.lex_health().unwrap();
            assert!(healthy.is_healthy());
            assert!(!healthy.is_dead());
            assert!(healthy.chunk_meta > 0);
        }
        let tantivy_dir = simulate_interrupted_lex_migration(dir.path());

        // Read the crippled state WITHOUT opening an Engine (which would heal
        // it): the two counts an Engine::lex_health would report.
        let lex_docs = LexIndex::open(&tantivy_dir).unwrap().num_docs().unwrap();
        let dead = LexHealth {
            lex_docs,
            chunk_meta: 1,
            rebuild_pending: lex::rebuild_pending(&tantivy_dir),
        };
        assert_eq!(dead.lex_docs, 0);
        assert!(
            dead.is_dead(),
            "empty lex + populated meta is the dead state"
        );
        assert!(dead.rebuild_pending);
        assert!(!dead.is_healthy());

        // An empty collection is NOT dead — no chunks means no missing chunks.
        let empty = LexHealth {
            lex_docs: 0,
            chunk_meta: 0,
            rebuild_pending: false,
        };
        assert!(!empty.is_dead());
        assert!(empty.is_healthy());
    }

    #[test]
    fn repopulate_skips_a_corrupt_chunk_meta_record() {
        // B1 part 3: `serde_json::from_str(..)?` inside the repopulate aborted
        // `Engine::open` itself — and it runs AFTER the wipe, so a single
        // unparseable row made the whole collection permanently unopenable and
        // its lex index permanently empty. Skip-not-fail, same convention as
        // `chunk_texts`: drop the bad chunk, keep the good ones, keep going.
        let dir = tempfile::tempdir().unwrap();
        {
            let mut e = fake_engine(dir.path());
            e.index_doc("rust.md", "# Rust\nerror handling and memory safety")
                .unwrap();
            e.index_doc("cook.md", "# Cooking\npasta recipe with tomato")
                .unwrap();
            // Genuinely malformed: not JSON at all, under a plausible key.
            let write_txn = e.meta.begin_write().unwrap();
            {
                let mut t = write_txn.open_table(CHUNK_META).unwrap();
                t.insert("broken.md#0", "{not json at all").unwrap();
            }
            write_txn.commit().unwrap();
        }

        let tantivy_dir = simulate_interrupted_lex_migration(dir.path());

        // Opening must SUCCEED despite the corrupt row.
        let e = fake_engine(dir.path());
        assert!(!lex::rebuild_pending(&tantivy_dir));
        // Both good docs survived the rebuild.
        assert!(!e.lex.search("error", 5).unwrap().is_empty());
        assert!(!e.lex.search("pasta", 5).unwrap().is_empty());
        // The corrupt one is simply absent — one fewer lex doc than rows.
        let health = e.lex_health().unwrap();
        assert_eq!(
            health.lex_docs + 1,
            health.chunk_meta,
            "exactly the corrupt row should be missing: {health:?}"
        );
        assert!(!health.is_dead());
    }

    #[test]
    fn repopulate_reports_progress() {
        // C6: a silent multi-minute rebuild reads as hung, and interrupting it
        // is what creates the state above. Progress must start at (0, total) —
        // before the first chunk — and reach (total, total).
        let dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(dir.path());
        e.index_doc("rust.md", "# Rust\nerror handling and memory safety")
            .unwrap();
        e.index_doc("cook.md", "# Cooking\npasta recipe with tomato")
            .unwrap();

        let mut seen: Vec<(usize, usize)> = Vec::new();
        let restored = e
            .repopulate_lex_from_meta_with_progress(&mut |done, total| seen.push((done, total)))
            .unwrap();
        let total = restored;
        assert!(total >= 2);
        assert_eq!(
            seen.first().copied(),
            Some((0, total)),
            "must announce the total before the first chunk: {seen:?}"
        );
        assert_eq!(seen.last().copied(), Some((total, total)));
        assert_eq!(seen.len(), total + 1, "one tick per chunk plus the opener");
    }

    #[test]
    fn active_model_matches_detects_change() {
        // No network needed: `open` never downloads a model, it only
        // records the requested model name in engine_header.
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(dir.path(), "multilingual-e5-small").unwrap();
        assert!(e.active_model_matches("multilingual-e5-small").unwrap());
        assert!(!e.active_model_matches("bge-m3").unwrap());
    }

    #[test]
    fn rebuild_reembeds_all() {
        if std::env::var("ONEBRAIN_TEST_EMBED").is_err() {
            return; // gated: downloads models (e5-small + e5-base)
        }
        let dir = tempfile::tempdir().unwrap();
        let mut e = Engine::open(dir.path(), "multilingual-e5-small").unwrap();
        e.index_doc("rust.md", "# Rust\nerror handling and memory safety")
            .unwrap();
        e.index_doc("cook.md", "# Cooking\npasta recipe with tomato")
            .unwrap();

        assert!(e.active_model_matches("multilingual-e5-small").unwrap());

        // Real dims change: e5-small is 384, e5-base is 768.
        let reembedded = e.rebuild("multilingual-e5-base").unwrap();
        assert_eq!(reembedded, 2);

        assert!(e.active_model_matches("multilingual-e5-base").unwrap());
        assert!(!e.active_model_matches("multilingual-e5-small").unwrap());

        // Vector store was rebuilt at the new dims and re-embedded: a query
        // still returns hits.
        let hits = e.query("memory safety", 3).unwrap();
        assert!(!hits.is_empty());
    }

    /// A single unreadable `.md` file must NOT abort the whole vault
    /// reindex: it's counted in `failed` and the loop continues. Non-gated —
    /// the failure happens at the file read, before any embedding, so no
    /// model download is triggered.
    #[cfg(unix)]
    #[test]
    fn reindex_all_counts_failed_and_continues() {
        extern "C" {
            fn geteuid() -> u32;
        }
        // chmod 000 is a no-op under root (read still succeeds), so the test
        // would spuriously see `failed: 0`. Skip when running as root.
        if unsafe { geteuid() } == 0 {
            return;
        }
        use std::os::unix::fs::PermissionsExt;

        let cache_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let mut e = Engine::open(cache_dir.path(), "multilingual-e5-small").unwrap();

        // One unreadable file (mode 000). Empty content would still need the
        // embedder if readable, but this file is never read past the failing
        // `std::fs::read`, so `failed` is incremented with no download.
        let bad = vault_dir.path().join("bad.md");
        std::fs::write(&bad, "# unreadable").unwrap();
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o000)).unwrap();

        let stats = e.reindex_all(vault_dir.path()).unwrap();
        // Restore perms so tempdir cleanup succeeds.
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert_eq!(stats.failed, 1, "unreadable file should count as failed");
        assert_eq!(stats.added, 0);
    }

    // ─────────────────────────────────────────────────────────────────────
    // #175 — engine-level reindex path confinement (direct path, not just HTTP)
    // ─────────────────────────────────────────────────────────────────────

    // `confine_reindex_doc_path` rejects the lexical escapes without touching
    // the filesystem beyond canonicalizing the (real) vault root.
    #[test]
    fn confine_rejects_parent_traversal() {
        let vault = tempfile::tempdir().unwrap();
        let err = confine_reindex_doc_path(vault.path(), "../../../etc/passwd").unwrap_err();
        assert!(err.contains("outside the vault"), "got {err:?}");
    }

    #[test]
    fn confine_rejects_absolute_path() {
        let vault = tempfile::tempdir().unwrap();
        let err = confine_reindex_doc_path(vault.path(), "/etc/passwd").unwrap_err();
        assert!(err.contains("outside the vault"), "got {err:?}");
    }

    #[test]
    fn confine_accepts_a_normal_nested_path() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join("01-projects")).unwrap();
        std::fs::write(vault.path().join("01-projects/a.md"), "x").unwrap();
        assert!(confine_reindex_doc_path(vault.path(), "01-projects/a.md").is_ok());
        // A not-yet-existing (removed) doc with a clean relative path is fine —
        // that's how a removal is expressed.
        assert!(confine_reindex_doc_path(vault.path(), "01-projects/gone.md").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn confine_rejects_symlink_escaping_the_vault() {
        // A symlink INSIDE the vault pointing OUTSIDE must be rejected by the
        // canonical-prefix check — the lexical `..` guard can't see through it.
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "top secret").unwrap();
        let vault = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            vault.path().join("leak.md"),
        )
        .unwrap();
        let err = confine_reindex_doc_path(vault.path(), "leak.md").unwrap_err();
        assert!(err.contains("outside the vault"), "got {err:?}");
    }

    // End-to-end via `reindex_paths`: a `..` escape and an absolute path are
    // each REJECTED (counted `failed`, `added == 0`), and — non-vacuously — the
    // real out-of-vault target is NEVER indexed/searchable. The rejection
    // happens before any read/embed, so no model download is triggered.
    #[test]
    fn reindex_paths_confines_escapes_and_indexes_nothing_out_of_vault() {
        let cache_dir = tempfile::tempdir().unwrap();
        // Nest the vault so `../secret.md` lands on a REAL seeded file that is
        // genuinely outside the vault root.
        let parent = tempfile::tempdir().unwrap();
        let vault_dir = parent.path().join("vault");
        std::fs::create_dir_all(&vault_dir).unwrap();
        let secret = parent.path().join("secret.md");
        std::fs::write(&secret, "# TOPSECRET_MARKER unique payload").unwrap();

        // Deterministic in-memory embedder — no model download, so `query`
        // works offline in CI (rejection happens before any embed anyway).
        let mut e = fake_engine(cache_dir.path());

        let escapes = vec![
            "../secret.md".to_string(),
            secret.to_string_lossy().into_owned(), // absolute path to the same file
            "../../etc/passwd".to_string(),
        ];
        let n = escapes.len();
        let stats = e.reindex_paths(&vault_dir, &escapes).unwrap();

        assert_eq!(stats.failed, n, "every escaping path must be rejected");
        assert_eq!(stats.added, 0, "nothing out-of-vault should be indexed");
        assert_eq!(stats.updated, 0);

        // Non-vacuous: the out-of-vault file must not be retrievable under ANY
        // of the keys used to smuggle it in, nor appear in a content search.
        for key in &escapes {
            assert!(e.stored_hash(key).unwrap().is_none(), "indexed: {key}");
            assert!(e.get(key).is_err(), "retrievable: {key}");
        }
        let hits = e.query("TOPSECRET_MARKER", 5).unwrap();
        assert!(
            hits.is_empty(),
            "out-of-vault content leaked into search: {} hits",
            hits.len()
        );
    }

    #[cfg(unix)]
    #[test]
    fn reindex_paths_confines_symlink_escape() {
        let cache_dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(
            outside.path().join("secret.txt"),
            "# LEAK_MARKER via symlink",
        )
        .unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            vault_dir.path().join("leak.md"),
        )
        .unwrap();

        let mut e = fake_engine(cache_dir.path());
        let paths = vec!["leak.md".to_string()];
        let stats = e.reindex_paths(vault_dir.path(), &paths).unwrap();

        assert_eq!(stats.failed, 1, "symlink escape must be rejected");
        assert_eq!(stats.added, 0);
        assert!(e.stored_hash("leak.md").unwrap().is_none());
        assert!(e.get("leak.md").is_err(), "symlinked file was retrievable");
        assert!(
            e.query("LEAK_MARKER", 5).unwrap().is_empty(),
            "symlinked out-of-vault content leaked into search"
        );
    }

    #[test]
    fn keep_top_cluster_keeps_within_window_drops_beyond() {
        // window 0.02 → keep scores >= top(0.839) - 0.02 = 0.819.
        let hits = vec![
            ("a".to_string(), 0.839f32),
            ("b".to_string(), 0.830),
            ("c".to_string(), 0.811),
        ];
        let kept = keep_top_cluster(hits, 0.02);
        assert_eq!(
            kept.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"],
            "keeps the top cluster (within 0.02 of top); drops c at 0.811"
        );
    }

    #[test]
    fn keep_top_cluster_never_empties_a_real_match() {
        // The exact e5 regression: a genuine top match scores 0.839 — BELOW the
        // old 0.85 floor, which silently returned zero hits. Relative keep must
        // RETURN it (recall-first).
        let hits = vec![("warm-daemon-doc".to_string(), 0.839f32)];
        assert_eq!(
            keep_top_cluster(hits, 0.02).len(),
            1,
            "a 0.839 real match must survive (old absolute floor dropped it)"
        );
    }

    #[test]
    fn keep_top_cluster_empty_stays_empty() {
        assert!(keep_top_cluster(Vec::<(String, f32)>::new(), 0.02).is_empty());
    }

    #[test]
    fn is_excluded_prefix_and_component_patterns() {
        let pats = vec!["attachments/demo".to_string(), "drafts".to_string()];
        assert!(is_excluded("attachments/demo/a.md", &pats));
        assert!(is_excluded("attachments/demo", &pats));
        assert!(
            !is_excluded("attachments/demo2/a.md", &pats),
            "no partial-prefix match"
        );
        assert!(
            is_excluded("x/drafts/a.md", &pats),
            "bare name matches any depth"
        );
        assert!(is_excluded("drafts/a.md", &pats));
        assert!(
            !is_excluded("notes/drafts.md", &pats),
            "'drafts.md' component does not match bare pattern 'drafts'"
        );
        assert!(!is_excluded("real.md", &pats));
        assert!(!is_excluded("real.md", &[]));
    }

    #[test]
    fn walk_honors_configured_exclude_patterns() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::write(vault.path().join("real.md"), "# real").unwrap();
        std::fs::create_dir_all(vault.path().join("attachments/demo")).unwrap();
        std::fs::write(vault.path().join("attachments/demo/skip.md"), "# skip").unwrap();

        let exclude = vec!["attachments".to_string()];
        let files = walk_markdown_files(vault.path(), &exclude).unwrap();
        let names: Vec<String> = files
            .iter()
            .filter_map(|f| vault_relative_path(vault.path(), f))
            .collect();
        assert_eq!(names, vec!["real.md".to_string()]);
    }

    #[test]
    fn walk_skips_hidden_dirs_and_node_modules() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::write(vault.path().join("real.md"), "# real").unwrap();
        for junk in [
            ".obsidian",
            ".git",
            "node_modules",
            "attachments/demo/node_modules",
        ] {
            let d = vault.path().join(junk);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("junk.md"), "# junk").unwrap();
        }
        std::fs::create_dir_all(vault.path().join("attachments/demo")).unwrap();
        std::fs::write(vault.path().join("attachments/demo/kept.md"), "# kept").unwrap();

        let files = walk_markdown_files(vault.path(), &[]).unwrap();
        let names: Vec<String> = files
            .iter()
            .filter_map(|f| vault_relative_path(vault.path(), f))
            .collect();
        assert!(names.contains(&"real.md".to_string()));
        assert!(names.contains(&"attachments/demo/kept.md".to_string()));
        assert!(
            names
                .iter()
                .all(|n| !n.contains("node_modules") && !n.starts_with('.')),
            "junk dirs must be skipped: {names:?}"
        );
    }

    #[test]
    fn vault_relative_path_strips_prefix_and_normalizes_slashes() {
        let root = Path::new("/vault/root");
        let file = Path::new("/vault/root/01-projects/onebrain/Project.md");
        assert_eq!(
            vault_relative_path(root, file).as_deref(),
            Some("01-projects/onebrain/Project.md")
        );

        // A path outside vault_root has no relative form.
        let outside = Path::new("/other/place.md");
        assert_eq!(vault_relative_path(root, outside), None);
    }

    #[test]
    fn short_path_hash_is_stable_and_six_hex_chars() {
        let p = Path::new("/Users/keng/vaults/ob-1");
        let h1 = short_path_hash(p);
        let h2 = short_path_hash(p);
        assert_eq!(h1, h2, "same path must hash identically");
        assert_eq!(h1.len(), 6);
        assert!(
            h1.chars().all(|c| c.is_ascii_hexdigit()),
            "hash must be hex: {h1}"
        );
        // Different paths give (with overwhelming probability) different hashes.
        assert_ne!(h1, short_path_hash(Path::new("/Users/keng/vaults/ob-2")));
    }

    #[test]
    fn hash_diff_decision_covers_all_cases() {
        assert_eq!(diff_hash(None, "abc"), HashDiff::Added);
        assert_eq!(diff_hash(Some("abc"), "abc"), HashDiff::Unchanged);
        assert_eq!(diff_hash(Some("abc"), "def"), HashDiff::Updated);
    }

    #[test]
    fn snippet_truncates_on_char_boundary() {
        let short = "hello world";
        assert_eq!(truncate_snippet(short, 200), short);

        let long_ascii = "a".repeat(250);
        let truncated = truncate_snippet(&long_ascii, 200);
        assert_eq!(truncated.chars().count(), 201); // 200 chars + "…"
        assert!(truncated.ends_with('…'));

        // Multibyte (Thai) text must not panic and must truncate on a char
        // boundary, not a byte offset.
        let thai = "ก".repeat(250);
        let truncated_thai = truncate_snippet(&thai, 200);
        assert_eq!(truncated_thai.chars().count(), 201);
        assert!(truncated_thai.ends_with('…'));
    }

    // -- Lex-only reindex (v3.4.5 Track 4) --------------------------------

    #[test]
    fn lex_only_reindex_never_embeds() {
        // A PanicEmbed engine: if `reindex_all_lex_only_with_progress` ever
        // touched the embedder (construct, embed, or dims), the test process
        // would panic instead of merely failing an assertion.
        let cache_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let mut e = panic_engine(cache_dir.path());

        std::fs::write(vault_dir.path().join("a.md"), "# A\nalpha content").unwrap();
        std::fs::write(vault_dir.path().join("b.md"), "# B\nbeta content").unwrap();

        let mut events = Vec::new();
        let stats = e
            .reindex_all_lex_only_with_progress(vault_dir.path(), &mut |p| events.push(p))
            .unwrap();
        assert_eq!(stats.added, 2);

        // A lex-only run never announces a model load — nothing is ever
        // embedded, so there's nothing to stall on.
        assert!(
            !events
                .iter()
                .any(|p| matches!(p, ReindexProgress::LoadingModel)),
            "lex-only run must never emit LoadingModel: {events:?}"
        );
    }

    #[test]
    fn lex_only_doc_is_lex_searchable_and_stays_pending() {
        let cache_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(cache_dir.path());

        std::fs::write(vault_dir.path().join("a.md"), "# A\nalpha content").unwrap();
        std::fs::write(vault_dir.path().join("b.md"), "# B\nbeta content").unwrap();

        let stats = e
            .reindex_all_lex_only_with_progress(vault_dir.path(), &mut |_| {})
            .unwrap();
        assert_eq!(stats.added, 2);

        // Chunk meta is present (lex-indexed) though never embedded/vector-current.
        assert!(e.get("a.md").unwrap().contains("alpha content"));
        assert!(e.get("b.md").unwrap().contains("beta content"));

        // DOC_HASHES was never written for either doc, so status still
        // reports both as pending-new, and doc_count (distinct DOC_HASHES
        // keys) is 0 — this is the crux of the whole feature.
        let status = e.status(vault_dir.path()).unwrap();
        assert_eq!(status.doc_count, 0);
        assert_eq!(status.pending_new, 2);
        assert_eq!(status.pending_total(), 2);
    }

    #[test]
    fn lex_only_second_run_is_unchanged() {
        let cache_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(cache_dir.path());

        std::fs::write(vault_dir.path().join("a.md"), "# A\nalpha content").unwrap();
        std::fs::write(vault_dir.path().join("b.md"), "# B\nbeta content").unwrap();

        let stats = e
            .reindex_all_lex_only_with_progress(vault_dir.path(), &mut |_| {})
            .unwrap();
        assert_eq!(stats.added, 2);

        // Second lex-only run: LEX_HASHES already holds both docs' current
        // hashes, so nothing is re-lexed.
        let stats2 = e
            .reindex_all_lex_only_with_progress(vault_dir.path(), &mut |_| {})
            .unwrap();
        assert_eq!(
            stats2,
            ReindexStats {
                added: 0,
                updated: 0,
                removed: 0,
                unchanged: 2,
                failed: 0,
            }
        );
    }

    #[test]
    fn full_reindex_after_lex_only_embeds_and_clears_pending() {
        let cache_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(cache_dir.path());

        std::fs::write(vault_dir.path().join("a.md"), "# A\nalpha content").unwrap();
        std::fs::write(vault_dir.path().join("b.md"), "# B\nbeta content").unwrap();

        e.reindex_all_lex_only_with_progress(vault_dir.path(), &mut |_| {})
            .unwrap();
        assert_eq!(e.status(vault_dir.path()).unwrap().pending_total(), 2);

        // A normal (full) reindex now embeds both docs and clears drift.
        let stats = e.reindex_all(vault_dir.path()).unwrap();
        assert_eq!(stats.added, 2);
        assert_eq!(e.status(vault_dir.path()).unwrap().pending_total(), 0);
        assert_eq!(e.status(vault_dir.path()).unwrap().doc_count, 2);

        // LEX_HASHES entries for both docs must be gone (Full mode drops
        // them so the effective-lex-hash fallback reads from DOC_HASHES): a
        // subsequent lex-only run reports them unchanged via the fallback,
        // not because a stale LEX_HASHES entry happens to still match.
        let stats2 = e
            .reindex_all_lex_only_with_progress(vault_dir.path(), &mut |_| {})
            .unwrap();
        assert_eq!(
            stats2,
            ReindexStats {
                added: 0,
                updated: 0,
                removed: 0,
                unchanged: 2,
                failed: 0,
            }
        );
    }

    #[test]
    fn lex_only_update_replaces_old_chunks() {
        let cache_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(cache_dir.path());

        let doc_path = vault_dir.path().join("a.md");
        std::fs::write(&doc_path, "# A\noriginal content").unwrap();

        // Full-index first (as if the doc was already fully embedded).
        let stats = e.reindex_all(vault_dir.path()).unwrap();
        assert_eq!(stats.added, 1);
        assert_eq!(e.status(vault_dir.path()).unwrap().pending_total(), 0);

        // Edit the file, then lex-only reindex.
        std::fs::write(&doc_path, "# A\nedited content, different bytes").unwrap();
        let stats2 = e
            .reindex_all_lex_only_with_progress(vault_dir.path(), &mut |_| {})
            .unwrap();
        assert_eq!(stats2.updated, 1);

        assert!(e.get("a.md").unwrap().contains("edited content"));
        // DOC_HASHES still holds the OLD (pre-edit) hash — Full mode never
        // ran again — so status reports the doc as pending_changed, proving
        // the hash-drift machinery still sees it as needing a real embed.
        let status = e.status(vault_dir.path()).unwrap();
        assert_eq!(status.pending_changed, 1);
        assert_eq!(status.pending_total(), 1);
    }

    #[test]
    fn full_reindex_after_lex_only_does_not_duplicate_lex_entries_for_new_doc() {
        // Regression for the lex-only -> pending-only handoff on a NEW doc:
        // (1) a lex-only pass indexes "a.md" and writes tantivy docs for
        // `a.md#0`, `a.md#1`, ... (LEX_HASHES only, no DOC_HASHES entry);
        // (2) a later Full reindex sees no DOC_HASHES entry, so it takes the
        // `Added` branch. `chunk_id` is deterministic (`{doc_path}#{idx}`)
        // and `LexIndex::add` never deletes-first, so without a `remove_doc`
        // first, Full's `index_doc` call would add a SECOND tantivy doc per
        // chunk_id on top of the lex-only one — corrupting the lex index
        // with duplicates that would double-count in `rrf_fuse`.
        let cache_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(cache_dir.path());

        let doc_path = vault_dir.path().join("a.md");
        std::fs::write(&doc_path, "# A\nalpha content unique_needle").unwrap();

        // Step 1: lex-only index the new doc.
        let stats = e
            .reindex_all_lex_only_with_progress(vault_dir.path(), &mut |_| {})
            .unwrap();
        assert_eq!(stats.added, 1);
        assert_eq!(e.status(vault_dir.path()).unwrap().pending_total(), 1);

        // Step 2: Full reindex (the "pending-only" embed pass).
        let stats2 = e.reindex_all(vault_dir.path()).unwrap();
        assert_eq!(stats2.added, 1);
        assert_eq!(e.status(vault_dir.path()).unwrap().pending_total(), 0);

        // Every chunk of a.md must appear exactly once in the lex index —
        // not duplicated by the lex-only add followed by an un-deduped
        // Full-mode add.
        let lex_hits = e.lex.search("unique_needle", 50).unwrap();
        let a_hits: Vec<&String> = lex_hits
            .iter()
            .map(|(chunk_id, _)| chunk_id)
            .filter(|id| id.starts_with("a.md#"))
            .collect();
        assert_eq!(
            a_hits.len(),
            1,
            "expected exactly one lex hit for a.md's single chunk, got {a_hits:?}"
        );

        let mut unique_ids: Vec<&&String> = a_hits.iter().collect();
        unique_ids.sort();
        unique_ids.dedup();
        assert_eq!(
            unique_ids.len(),
            a_hits.len(),
            "lex hits for a.md must not contain duplicate chunk_ids"
        );
    }

    #[test]
    fn lex_only_removal_sweeps_gone_docs() {
        let cache_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(cache_dir.path());

        let a = vault_dir.path().join("a.md");
        let b = vault_dir.path().join("b.md");
        std::fs::write(&a, "# A\nalpha content").unwrap();
        std::fs::write(&b, "# B\nbeta content").unwrap();

        // Full-index both.
        let stats = e.reindex_all(vault_dir.path()).unwrap();
        assert_eq!(stats.added, 2);
        assert_eq!(e.status(vault_dir.path()).unwrap().pending_total(), 0);

        // Delete one file, lex-only reindex_all: it must be swept (both hash
        // tables dropped) just like a full reindex would.
        std::fs::remove_file(&a).unwrap();
        let stats2 = e
            .reindex_all_lex_only_with_progress(vault_dir.path(), &mut |_| {})
            .unwrap();
        assert_eq!(stats2.removed, 1);

        assert!(e.get("a.md").is_err());
        let status = e.status(vault_dir.path()).unwrap();
        assert_eq!(
            status.pending_total(),
            0,
            "both hash entries must be dropped for a.md"
        );
        assert_eq!(status.doc_count, 1, "only b.md remains indexed");
    }

    #[test]
    fn lex_only_reindex_paths_targets_specific_docs() {
        // Same coverage as `lex_only_reindex_never_embeds` but through the
        // targeted-paths entry point, proving both public APIs share the
        // mode plumbing.
        let cache_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let mut e = panic_engine(cache_dir.path());

        std::fs::write(vault_dir.path().join("a.md"), "# A\nalpha content").unwrap();

        let stats = e
            .reindex_paths_lex_only_with_progress(
                vault_dir.path(),
                &["a.md".to_string()],
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(stats.added, 1);
        assert!(e.get("a.md").unwrap().contains("alpha content"));
    }

    #[test]
    fn pending_vector_paths_lists_lex_only_docs() {
        // A lex-only reindex deliberately leaves DOC_HASHES untouched, so
        // both docs must show up in the deferred-embed worklist even though
        // they're fully lex-searchable.
        let cache_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(cache_dir.path());

        std::fs::write(vault_dir.path().join("a.md"), "# A\nalpha content").unwrap();
        std::fs::write(vault_dir.path().join("b.md"), "# B\nbeta content").unwrap();

        let stats = e
            .reindex_all_lex_only_with_progress(vault_dir.path(), &mut |_| {})
            .unwrap();
        assert_eq!(stats.added, 2);

        let mut pending = e.pending_vector_paths(vault_dir.path()).unwrap();
        pending.sort();
        assert_eq!(pending, vec!["a.md".to_string(), "b.md".to_string()]);

        // A real (full) reindex embeds both docs and clears the worklist.
        e.reindex_all(vault_dir.path()).unwrap();
        assert_eq!(
            e.pending_vector_paths(vault_dir.path()).unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn pending_vector_paths_includes_changed_and_removed() {
        let cache_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(cache_dir.path());

        let a = vault_dir.path().join("a.md");
        let b = vault_dir.path().join("b.md");
        let c = vault_dir.path().join("c.md");
        std::fs::write(&a, "# A\nalpha content").unwrap();
        std::fs::write(&b, "# B\nbeta content").unwrap();
        std::fs::write(&c, "# C\ngamma content").unwrap();
        e.reindex_all(vault_dir.path()).unwrap();
        assert_eq!(
            e.pending_vector_paths(vault_dir.path()).unwrap(),
            Vec::<String>::new()
        );

        // Modify a, delete b, leave c untouched.
        std::fs::write(&a, "# A\nalpha content EDITED").unwrap();
        std::fs::remove_file(&b).unwrap();

        let mut pending = e.pending_vector_paths(vault_dir.path()).unwrap();
        pending.sort();
        assert_eq!(pending, vec!["a.md".to_string(), "b.md".to_string()]);
    }

    #[test]
    fn pending_vector_paths_empty_on_fresh_index_current_vault() {
        let cache_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(cache_dir.path());

        std::fs::write(vault_dir.path().join("a.md"), "# A\nalpha content").unwrap();
        std::fs::write(vault_dir.path().join("b.md"), "# B\nbeta content").unwrap();
        e.reindex_all(vault_dir.path()).unwrap();

        assert_eq!(
            e.pending_vector_paths(vault_dir.path()).unwrap(),
            Vec::<String>::new()
        );
    }
    // ─────────────────────────────────────────────────────────────────
    // Rerank stage: pipeline + gate + never-empty + fallback
    // ─────────────────────────────────────────────────────────────────

    use crate::rerank::{FakeReranker, Rerank};

    /// A reranker whose every call fails. Proves queries NEVER fail because
    /// reranking failed — the engine must fall back to fused order.
    struct FailingReranker;

    impl Rerank for FailingReranker {
        fn rerank(&self, _query: &str, _passages: &[String]) -> Result<Vec<f32>> {
            anyhow::bail!("FailingReranker always fails")
        }
    }

    /// A reranker with a hardcoded preference: passages containing `marker`
    /// score high, everything else low. Used where a deterministic
    /// retrieval-vs-rerank disagreement is needed — on the pure vector path
    /// [`FakeReranker`]'s Jaccard correlates with [`FakeEmbedder`]'s cosine
    /// (both measure token overlap with the query), so it cannot flip a
    /// cosine ordering there.
    struct MarkerReranker {
        marker: &'static str,
    }

    impl Rerank for MarkerReranker {
        fn rerank(&self, _query: &str, passages: &[String]) -> Result<Vec<f32>> {
            Ok(passages
                .iter()
                .map(|p| if p.contains(self.marker) { 0.9 } else { 0.4 })
                .collect())
        }
    }

    /// Like [`MarkerReranker`] but with scores straddling the SHIPPED gate,
    /// using the bands the v3.4.7 calibration actually measured on real vault
    /// content: genuine matches 0.73–0.99, non-matches 0.003–0.066.
    /// `MarkerReranker`'s 0.4 "weak" score clears the 0.30 default, so it
    /// cannot exercise gate behavior at default settings at all.
    struct StraddlingReranker {
        marker: &'static str,
    }

    impl Rerank for StraddlingReranker {
        fn rerank(&self, _query: &str, passages: &[String]) -> Result<Vec<f32>> {
            Ok(passages
                .iter()
                .map(|p| if p.contains(self.marker) { 0.9 } else { 0.05 })
                .collect())
        }
    }

    /// Records how many passages it was asked to cross-encode (total across
    /// calls), and scores them all high so none is gated out. Used to assert
    /// `rerank_paths` bounds the number of chunks it reranks.
    struct CountingReranker {
        seen: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Rerank for CountingReranker {
        fn rerank(&self, _query: &str, passages: &[String]) -> Result<Vec<f32>> {
            self.seen
                .fetch_add(passages.len(), std::sync::atomic::Ordering::Relaxed);
            Ok(vec![0.9; passages.len()])
        }
    }

    /// Ten distinct query tokens for the rerank reorder scenarios.
    const RERANK_QUERY: &str = "alpha bravo charlie delta echo foxtrot golf hotel india juliet";

    /// Two-doc corpus engineered so retrieval order and rerank order
    /// disagree (see math in header note).
    fn rerank_corpus(e: &mut Engine) {
        let bait = format!("{RERANK_QUERY} {RERANK_QUERY} zulumark");
        e.index_doc("bait.md", &bait).unwrap();
        e.index_doc("target.md", RERANK_QUERY).unwrap();
    }

    #[test]
    fn rerank_reorders_hybrid_hits_with_descending_scores() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(dir.path());
        e.index_doc("a.md", &format!("{RERANK_QUERY} alphamark"))
            .unwrap();
        e.index_doc("b.md", &format!("{RERANK_QUERY} betamark"))
            .unwrap();

        // Precondition: without a reranker (lazy source, model not
        // downloaded into this temp cache dir) nothing carries a rerank
        // score — i.e. today's pipeline.
        let before = e.query(RERANK_QUERY, 5).unwrap();
        assert_eq!(before.len(), 2);
        assert!(before.iter().all(|h| h.rerank_score.is_none()));

        // Whichever doc RRF ranks SECOND, a reranker that prefers it must
        // put it first — proving rerank order wins over fused order without
        // depending on BM25 length-normalization details (an earlier fixture
        // that predicted the exact BM25 winner got them wrong).
        let loser = before[1].doc_path.clone();
        let marker: &'static str = if loser == "a.md" {
            "alphamark"
        } else {
            "betamark"
        };

        e.set_reranker_for_tests(Box::new(MarkerReranker { marker }));
        let hits = e.query(RERANK_QUERY, 5).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0].doc_path, loser,
            "reranker order must win over RRF order"
        );
        for h in &hits {
            let s = h
                .rerank_score
                .expect("every reranked hit carries Some(score)");
            assert!(s > 0.0 && s < 1.0, "score {s} out of (0,1)");
        }
        assert!(
            hits[0].rerank_score.unwrap() > hits[1].rerank_score.unwrap(),
            "reranked block must be sorted descending by rerank score"
        );
    }

    #[test]
    fn rerank_gate_keeps_top_three_when_nothing_clears_min_score() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(dir.path());
        for i in 0..5 {
            e.index_doc(&format!("d{i}.md"), &format!("note filler{i}a filler{i}b"))
                .unwrap();
        }
        // FakeReranker scores are strictly below sigmoid(2) ≈ 0.881, so a
        // 0.999 gate rejects everything → never-empty keeps exactly
        // RERANK_NO_MATCH_KEEP.
        e.set_rerank_settings(RerankSettings {
            min_score: 0.999,
            ..Default::default()
        });
        e.set_reranker_for_tests(Box::new(FakeReranker));
        let hits = e.query("note", 10).unwrap();
        assert_eq!(hits.len(), 3, "gate rejects all → keep exactly top 3");
        assert!(hits.iter().all(|h| h.rerank_score.is_some()));

        // Fewer candidates than the never-empty floor: keep all n, not 3.
        let dir2 = tempfile::tempdir().unwrap();
        let mut e2 = fake_engine(dir2.path());
        e2.index_doc("a.md", "note aa ab").unwrap();
        e2.index_doc("b.md", "note ba bb").unwrap();
        e2.set_rerank_settings(RerankSettings {
            min_score: 0.999,
            ..Default::default()
        });
        e2.set_reranker_for_tests(Box::new(FakeReranker));
        let hits2 = e2.query("note", 10).unwrap();
        assert_eq!(hits2.len(), 2, "min(3, n) with n = 2");
        assert!(hits2.iter().all(|h| h.rerank_score.is_some()));
    }

    #[test]
    fn rerank_failure_falls_back_to_fused_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(dir.path());
        rerank_corpus(&mut e);
        let before: Vec<String> = e
            .query(RERANK_QUERY, 5)
            .unwrap()
            .into_iter()
            .map(|h| h.doc_path)
            .collect();

        e.set_reranker_for_tests(Box::new(FailingReranker));
        let hits = e.query(RERANK_QUERY, 5).unwrap();
        let after: Vec<String> = hits.iter().map(|h| h.doc_path.clone()).collect();
        assert_eq!(after, before, "reranker failure must preserve RRF order");
        assert!(
            hits.iter().all(|h| h.rerank_score.is_none()),
            "failed rerank leaves every score None"
        );
    }

    #[test]
    fn rerank_disabled_or_absent_matches_unreranked_pipeline() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(dir.path());
        e.index_doc("rust.md", "# Rust\nerror handling and memory safety")
            .unwrap();
        e.index_doc("cook.md", "# Cooking\npasta recipe with tomato")
            .unwrap();

        // Absent: default settings (enabled) but the lazy reranker's model
        // is not downloaded → silently skipped. Pre-change snapshot
        // expectation: rust.md tops the hybrid ranking (same assertion as
        // fake_index_doc_then_query_returns_expected_top_hit).
        let absent = e.query("memory safety", 3).unwrap();
        assert!(!absent.is_empty());
        assert_eq!(absent[0].doc_path, "rust.md");
        assert!(absent.iter().all(|h| h.rerank_score.is_none()));

        // Disabled: enabled=false must gate even an injected reranker.
        e.set_rerank_settings(RerankSettings {
            enabled: false,
            ..Default::default()
        });
        e.set_reranker_for_tests(Box::new(FakeReranker));
        let disabled = e.query("memory safety", 3).unwrap();
        assert_eq!(
            disabled.iter().map(|h| &h.doc_path).collect::<Vec<_>>(),
            absent.iter().map(|h| &h.doc_path).collect::<Vec<_>>(),
            "disabled rerank must leave the pipeline unchanged"
        );
        assert!(disabled.iter().all(|h| h.rerank_score.is_none()));
    }

    #[test]
    fn rerank_vector_search_reorders_and_gates() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(dir.path());
        rerank_corpus(&mut e);

        // Precondition: pure cosine ranks the exact-text target doc first
        // (1.0 vs ≈ 0.988 for the doubled bait — which must survive the
        // 0.02 cluster window).
        let before = e.vector_search(RERANK_QUERY, 5).unwrap();
        assert_eq!(before.len(), 2, "bait must survive the cluster window");
        assert_eq!(before[0].doc_path, "target.md");
        assert!(before.iter().all(|h| h.rerank_score.is_none()));

        // MarkerReranker favors the bait doc → rerank order must win.
        e.set_reranker_for_tests(Box::new(MarkerReranker { marker: "zulumark" }));
        let hits = e.vector_search(RERANK_QUERY, 5).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0].doc_path, "bait.md",
            "rerank order must win over cosine order"
        );
        assert!(hits[0].rerank_score.unwrap() > hits[1].rerank_score.unwrap());

        // Gate on the vector path: nothing clears an absurd threshold →
        // never-empty keeps min(3, n).
        let dir2 = tempfile::tempdir().unwrap();
        let mut e2 = fake_engine(dir2.path());
        for i in 0..4 {
            e2.index_doc(&format!("s{i}.md"), "note common tokens")
                .unwrap();
        }
        e2.set_rerank_settings(RerankSettings {
            min_score: 0.999,
            ..Default::default()
        });
        e2.set_reranker_for_tests(Box::new(FakeReranker));
        let gated = e2.vector_search("note common tokens", 10).unwrap();
        assert_eq!(gated.len(), 3, "vsearch gate keeps exactly top 3");
        assert!(gated.iter().all(|h| h.rerank_score.is_some()));
    }

    #[test]
    fn rerank_tail_beyond_top_k_is_dropped_by_truncation() {
        // `min_candidates` is a FLOOR, auto-raised to `top_k`
        // (`max(min_candidates, top_k)`): with min_candidates=2 but top_k=4
        // on a 6-doc corpus, the reranked pool covers all 4 returned hits,
        // and the fused tail beyond top_k (docs 5-6) is dropped by
        // truncation — it never appears unreranked within the returned set.
        let dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(dir.path());
        for i in 0..6 {
            e.index_doc(&format!("t{i}.md"), &format!("zeta unique{i}"))
                .unwrap();
        }
        e.set_rerank_settings(RerankSettings {
            min_candidates: 2,
            min_score: 0.0,
            ..Default::default()
        });
        e.set_reranker_for_tests(Box::new(FakeReranker));
        let hits = e.query("zeta", 4).unwrap();
        assert_eq!(
            hits.len(),
            4,
            "truncated to top_k, not left at min_candidates"
        );
        assert!(
            hits.iter().all(|h| h.rerank_score.is_some()),
            "every returned hit is reranked — pool size is max(min_candidates, top_k), not min_candidates alone"
        );
    }

    #[test]
    fn rerank_pool_covers_full_top_k_when_top_k_exceeds_min_candidates() {
        // Correctness fix: the rerank window must be
        // max(min_candidates, top_k), never just `min_candidates`. With
        // min_candidates=2 and top_k=5 on a 6-doc corpus, all 5 returned
        // results must carry rerank_score: Some — none may slip through
        // unreranked within the returned top_k.
        let dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(dir.path());
        for i in 0..6 {
            e.index_doc(&format!("d{i}.md"), &format!("zeta unique{i}"))
                .unwrap();
        }
        e.set_rerank_settings(RerankSettings {
            min_candidates: 2,
            min_score: 0.0,
            ..Default::default()
        });
        e.set_reranker_for_tests(Box::new(FakeReranker));
        let hits = e.query("zeta", 5).unwrap();
        assert_eq!(hits.len(), 5);
        assert!(
            hits.iter().all(|h| h.rerank_score.is_some()),
            "no unreranked result may appear within the returned top_k"
        );
    }

    #[test]
    fn rerank_gate_drops_failing_candidates_without_backfill() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(dir.path());
        e.index_doc("m1.md", "note zulumark alpha").unwrap();
        e.index_doc("m2.md", "note zulumark beta").unwrap();
        for i in 0..3 {
            e.index_doc(&format!("u{i}.md"), &format!("note gamma{i}"))
                .unwrap();
        }
        e.set_rerank_settings(RerankSettings {
            min_score: 0.5,
            ..Default::default()
        });
        e.set_reranker_for_tests(Box::new(MarkerReranker { marker: "zulumark" }));
        let hits = e.query("note", 10).unwrap();
        // Partial gate: only the two marker docs clear 0.5 (0.9 vs 0.4).
        // Gate-dropped candidates are REMOVED, never backfilled — the result
        // shrinks below top_k. (The fused tail beyond `min_candidates` is a
        // separate mechanism, covered by the test above.) Track B confidence
        // bands depend on this exact semantic.
        assert_eq!(hits.len(), 2, "gate-dropped candidates must not reappear");
        assert!(hits.iter().all(|h| h.doc_path.starts_with("m")));
        assert!(hits.iter().all(|h| h.rerank_score == Some(0.9)));
    }

    #[test]
    fn default_gate_demotes_weak_candidates_instead_of_dropping_them() {
        // Measured 2026-07-19 on a real 782-doc vault: the shipped 0.30 gate
        // removed HALF the correct answers (heading-shaped probes hit@10
        // 0.500 → 0.233; body-term probes 0.733 → 0.500). Partial rejection —
        // not the total rejection `RERANK_NO_MATCH_KEEP` guards — was the
        // recall killer, because `candidates` and `top_k` are both 10, leaving
        // no fused tail to backfill with.
        //
        // The default therefore no longer truncates. The block is already
        // sorted by cross-encoder score, so weak candidates simply rank BELOW
        // strong ones and every hit still carries `rerank_score` — which is
        // what the search cascade tells the agent to judge confidence on.
        let dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(dir.path());
        e.index_doc("m1.md", "note zulumark alpha").unwrap();
        e.index_doc("m2.md", "note zulumark beta").unwrap();
        for i in 0..3 {
            e.index_doc(&format!("u{i}.md"), &format!("note gamma{i}"))
                .unwrap();
        }
        // No set_rerank_settings call: this pins the SHIPPED default.
        e.set_reranker_for_tests(Box::new(StraddlingReranker { marker: "zulumark" }));
        let hits = e.query("note", 10).unwrap();

        assert_eq!(
            hits.len(),
            5,
            "weak candidates must survive as demoted hits"
        );
        assert!(
            hits[0].doc_path.starts_with('m') && hits[1].doc_path.starts_with('m'),
            "strong matches must still rank first, got {:?}",
            hits.iter().map(|h| &h.doc_path).collect::<Vec<_>>()
        );
        assert!(
            hits[2..].iter().all(|h| h.doc_path.starts_with('u')),
            "weak matches must be demoted below strong ones"
        );
        assert!(
            hits.iter().all(|h| h.rerank_score.is_some()),
            "every hit keeps its score so the agent can judge confidence"
        );
    }

    #[test]
    fn set_rerank_min_score_overrides_the_gate_per_query() {
        // Backs the CLI `--min-score` unification: raising the gate via
        // `set_rerank_min_score` filters the calibrated rerank_score, and the
        // never-empty floor still applies when everything is dropped.
        let dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(dir.path());
        e.index_doc("m1.md", "note zulumark alpha").unwrap();
        e.index_doc("m2.md", "note zulumark beta").unwrap();
        for i in 0..3 {
            e.index_doc(&format!("u{i}.md"), &format!("note gamma{i}"))
                .unwrap();
        }
        e.set_reranker_for_tests(Box::new(MarkerReranker { marker: "zulumark" }));

        // Default gate (0.30) keeps the two marker docs (0.9); the 0.4 tail is
        // below the MarkerReranker's non-marker score only above 0.4 — verify
        // the override tightens past 0.9 → nothing clears → never-empty floor.
        e.set_rerank_min_score(0.95);
        let hits = e.query("note", 10).unwrap();
        assert_eq!(
            hits.len(),
            RERANK_NO_MATCH_KEEP,
            "gate at 0.95 drops all (max score 0.9) → never-empty floor keeps top-3"
        );

        // Loosen the gate below every score → all reranked candidates survive.
        e.set_rerank_min_score(0.0);
        let hits = e.query("note", 10).unwrap();
        assert!(
            hits.len() >= 2 && hits.iter().all(|h| h.rerank_score.is_some()),
            "gate at 0.0 keeps every reranked hit"
        );
    }

    #[test]
    fn rerank_active_tracks_settings_and_reranker_availability() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(dir.path());
        // Lazy source, model not downloaded into this temp cache dir.
        assert!(!e.rerank_active());
        e.set_reranker_for_tests(Box::new(FakeReranker));
        assert!(e.rerank_active(), "injected reranker → active");
        e.set_rerank_settings(RerankSettings {
            enabled: false,
            ..Default::default()
        });
        assert!(!e.rerank_active(), "disabled settings → inactive");
    }

    // ─────────────────────────────────────────────────────────────────
    // Public rerank entry points: rerank_hits (chunk-level), rerank_paths
    // (doc-level)
    // ─────────────────────────────────────────────────────────────────

    #[test]
    fn rerank_hits_matches_apply_rerank_reorder() {
        // rerank_hits is a thin pub wrapper over apply_rerank — same
        // reorder-wins-over-fused-order behavior as the private-method test
        // above (rerank_reorders_hybrid_hits_with_descending_scores), just
        // invoked through the public surface with a caller-supplied fused
        // list rather than one built internally by `query`.
        let dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(dir.path());
        e.index_doc("a.md", &format!("{RERANK_QUERY} alphamark"))
            .unwrap();
        e.index_doc("b.md", &format!("{RERANK_QUERY} betamark"))
            .unwrap();

        // Fetch the real fused-order chunk ids/scores via the existing
        // pipeline (chunk ids are content-derived, not a predictable
        // "doc.md#N" format) so we have a realistic fused list to hand to
        // the public wrapper.
        let before = e.query(RERANK_QUERY, 5).unwrap();
        assert_eq!(before.len(), 2);

        let loser = before[1].doc_path.clone();
        let marker: &'static str = if loser == "a.md" {
            "alphamark"
        } else {
            "betamark"
        };
        e.set_reranker_for_tests(Box::new(MarkerReranker { marker }));

        let fused: Vec<(String, f64)> = before
            .iter()
            .map(|h| (h.chunk_id.clone(), h.score))
            .collect();
        let hits = e.rerank_hits(RERANK_QUERY, fused, 5).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0].doc_path, loser,
            "rerank_hits must apply the same reorder as apply_rerank"
        );
        assert!(hits.iter().all(|h| h.rerank_score.is_some()));
    }

    #[test]
    fn rerank_hits_returns_fused_order_when_reranker_absent() {
        // Skip-not-fail at the public surface: no reranker configured (lazy
        // source, model not downloaded in this temp cache dir) must return
        // the input fused order untouched with rerank_score: None, never
        // panic or error.
        let dir = tempfile::tempdir().unwrap();
        let e = fake_engine(dir.path());
        let fused = vec![("chunk-b".to_string(), 2.0), ("chunk-a".to_string(), 1.0)];
        let hits = e.rerank_hits("anything", fused.clone(), 10).unwrap();
        // No chunk meta exists for these synthetic ids, so resolve_hits
        // skips them all — the point of this test is that the call
        // completes without error and yields no rerank scores, not that
        // hits are non-empty.
        assert!(hits.is_empty());

        // Repeat with real, indexed chunks so we can check order + None.
        let dir2 = tempfile::tempdir().unwrap();
        let mut e2 = fake_engine(dir2.path());
        e2.index_doc("a.md", "alpha content").unwrap();
        e2.index_doc("b.md", "beta content").unwrap();
        let before = e2.query("alpha beta", 5).unwrap();
        let fused2: Vec<(String, f64)> = before
            .iter()
            .map(|h| (h.chunk_id.clone(), h.score))
            .collect();
        let hits2 = e2.rerank_hits("alpha beta", fused2, 5).unwrap();
        assert_eq!(
            hits2.iter().map(|h| &h.doc_path).collect::<Vec<_>>(),
            before.iter().map(|h| &h.doc_path).collect::<Vec<_>>(),
            "no reranker → fused order preserved"
        );
        assert!(hits2.iter().all(|h| h.rerank_score.is_none()));
    }

    #[test]
    fn rerank_paths_dedups_to_best_chunk_per_path() {
        // Two docs, each with enough distinct content to form 2+ chunks is
        // overkill here — instead use MarkerReranker across a doc whose
        // single chunk should out-score another doc's single chunk, then
        // verify dedup keeps exactly one Hit per doc_path (the common case
        // is one chunk per short doc, so this also covers the trivial
        // single-chunk-per-path dedup path).
        let dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(dir.path());
        e.index_doc("m1.md", "note zulumark alpha").unwrap();
        e.index_doc("m2.md", "note plain beta").unwrap();
        e.set_reranker_for_tests(Box::new(MarkerReranker { marker: "zulumark" }));

        let paths = vec!["m1.md".to_string(), "m2.md".to_string()];
        let hits = e.rerank_paths("note", paths, 10).unwrap();

        // One Hit per path.
        let mut seen_paths: Vec<&str> = hits.iter().map(|h| h.doc_path.as_str()).collect();
        seen_paths.sort();
        seen_paths.dedup();
        assert_eq!(
            seen_paths.len(),
            hits.len(),
            "rerank_paths must return at most one Hit per doc_path"
        );
        assert_eq!(hits.len(), 2, "both candidate paths must survive dedup");

        // The marker doc must rank first (0.9 vs 0.4) and its rerank_score
        // must be the highest among its own chunks (trivially true here
        // since each doc has one chunk, but exercises the "keep best"
        // comparison path).
        assert_eq!(hits[0].doc_path, "m1.md");
        assert_eq!(hits[0].rerank_score, Some(0.9));
    }

    #[test]
    fn rerank_paths_truncates_to_top_k_after_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(dir.path());
        for i in 0..5 {
            e.index_doc(&format!("p{i}.md"), &format!("note common{i}"))
                .unwrap();
        }
        e.set_reranker_for_tests(Box::new(FakeReranker));

        let paths: Vec<String> = (0..5).map(|i| format!("p{i}.md")).collect();
        let hits = e.rerank_paths("note common", paths, 2).unwrap();
        assert_eq!(hits.len(), 2, "top_k truncation applies after dedup");
    }

    #[test]
    fn rerank_paths_bounds_total_chunks_to_budget() {
        // Long, multi-section docs → many chunks each. rerank_paths must
        // cross-encode at most budget = max(min_candidates, top_k) chunks
        // TOTAL (gathered round-robin), never "every chunk of every doc" —
        // that is the whole point of the bound on long-document vaults.
        let dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(dir.path());
        let body: String = (0..12)
            .map(|j| {
                format!("## Section {j}\ncontent about search and rerank paragraph number {j} with several extra filler words here\n\n")
            })
            .collect();
        for i in 0..3 {
            e.index_doc(&format!("d{i}.md"), &format!("# Doc {i}\n{body}"))
                .unwrap();
        }
        // Precondition: the corpus must actually chunk into MORE than budget,
        // or the bound would be exercised only vacuously.
        let total_chunks: usize = (0..3)
            .map(|i| e.doc_chunk_ids(&format!("d{i}.md")).unwrap().len())
            .sum();
        assert!(
            total_chunks > 4,
            "test corpus must produce >budget chunks to be meaningful, got {total_chunks}"
        );

        // budget = max(min_candidates, top_k) = max(4, 4) = 4.
        e.set_rerank_settings(RerankSettings {
            enabled: true,
            min_candidates: 4,
            ..Default::default()
        });
        let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        e.set_reranker_for_tests(Box::new(CountingReranker { seen: seen.clone() }));

        let paths: Vec<String> = (0..3).map(|i| format!("d{i}.md")).collect();
        let hits = e.rerank_paths("search rerank", paths, 4).unwrap();

        let seen = seen.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            seen <= 4,
            "must cross-encode at most budget (4) chunks regardless of doc length, saw {seen}"
        );
        // Round-robin fairness: with 3 paths and budget 4, round 0 reranks
        // every path's first chunk — so every candidate path is represented.
        let mut paths_seen: Vec<&str> = hits.iter().map(|h| h.doc_path.as_str()).collect();
        paths_seen.sort();
        paths_seen.dedup();
        assert_eq!(
            paths_seen.len(),
            3,
            "round-robin must rerank at least one chunk of every candidate path"
        );
    }

    #[test]
    fn rerank_paths_returns_fused_order_when_reranker_absent() {
        // Skip-not-fail at the doc-level entry point too: no reranker
        // configured → every chunk carries rerank_score: None and the call
        // must not error even though `apply_rerank`'s internal gate/sort
        // logic is bypassed entirely in that branch.
        let dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(dir.path());
        e.index_doc("a.md", "alpha content").unwrap();
        e.index_doc("b.md", "beta content").unwrap();

        let paths = vec!["a.md".to_string(), "b.md".to_string()];
        let hits = e.rerank_paths("alpha beta", paths, 10).unwrap();
        assert_eq!(
            hits.len(),
            2,
            "both paths present, no reranker to gate them out"
        );
        assert!(hits.iter().all(|h| h.rerank_score.is_none()));

        let mut seen: Vec<&str> = hits.iter().map(|h| h.doc_path.as_str()).collect();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 2, "still one Hit per path with no reranker");
    }

    #[test]
    fn rerank_paths_empty_paths_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let e = fake_engine(dir.path());
        let hits = e.rerank_paths("anything", Vec::new(), 10).unwrap();
        assert!(hits.is_empty(), "no candidate paths → no hits, no panic");
    }

    /// Opening an engine on a collection whose artifacts are still at the
    /// legacy flat root must migrate them under `index/` and keep the existing
    /// index intact — the doc stays searchable, its stored vector survives
    /// (exact-text cosine ~1.0), and `doc_count` is unchanged (no wipe, no
    /// re-embed). This is the integration guarantee behind PR-5b.
    #[test]
    fn engine_open_migrates_legacy_layout_without_reindex() {
        let cache = tempfile::tempdir().unwrap();
        let root = cache.path();
        let vault = tempfile::tempdir().unwrap();
        std::fs::write(
            vault.path().join("rust.md"),
            "# Rust\nerror handling and memory safety",
        )
        .unwrap();

        // 1. Build a real tiny index with the current API (reindex_all records
        //    DOC_HASHES, so doc_count is meaningful), then drop it to release
        //    redb's single-process lock.
        let doc_count_before = {
            let mut e = fake_engine(root);
            e.skip_reranker_fetch_for_tests(); // no ~570 MB hf-hub download in a unit test
            e.reindex_all(vault.path()).unwrap();
            let s = e.status(vault.path()).unwrap();
            assert_eq!(s.doc_count, 1);
            assert_eq!(s.pending_total(), 0);
            s.doc_count
        };

        // 2. Fake a legacy flat layout: move every index artifact from wherever
        //    it lives back to the collection root, and clear the split dirs so
        //    the collection looks genuinely un-migrated. (Artifact names come
        //    from a variable so this test carries no literal `index/<name>`
        //    path — the layout module owns those.)
        let artifacts = ["tantivy", "vectors", "engine.redb"];
        let index_dir = root.join("index");
        for name in artifacts {
            let from = CollectionLayout::new(root).index_artifact(name);
            if from != root.join(name) && from.exists() {
                std::fs::rename(&from, root.join(name)).unwrap();
            }
        }
        let _ = std::fs::remove_dir_all(&index_dir);
        for name in artifacts {
            assert!(
                root.join(name).exists(),
                "precondition: {name} must sit at the legacy root"
            );
            assert!(!index_dir.join(name).exists());
        }

        // 3. Reopen → open_inner migrates eagerly.
        let e = fake_engine(root);
        for name in artifacts {
            assert!(
                index_dir.join(name).exists(),
                "{name} must move under index/ on open"
            );
            assert!(
                !root.join(name).exists(),
                "legacy {name} at root must be gone after migration"
            );
        }

        // The index survived intact: the doc is still searchable, its stored
        // vector was reused (exact-text cosine ~1.0), doc_count is unchanged,
        // and nothing reads as pending — i.e. no wipe and no re-embed.
        let hits = e
            .vector_search("error handling and memory safety", 3)
            .unwrap();
        assert!(hits.iter().any(|h| h.doc_path == "rust.md"));
        assert!(hits[0].score > 0.99, "score was {}", hits[0].score);

        let after = e.status(vault.path()).unwrap();
        assert_eq!(
            after.doc_count, doc_count_before,
            "doc_count must survive migration (no re-embed / wipe)"
        );
        assert_eq!(
            after.pending_total(),
            0,
            "nothing should read as pending after migration — vectors were reused, not rebuilt"
        );
    }

    // ---- Track 3: generation counter + doc_hash accessor (design §3) ----

    #[test]
    fn generation_starts_at_zero_before_any_reindex() {
        let cache_dir = tempfile::tempdir().unwrap();
        let e = fake_engine(cache_dir.path());
        assert_eq!(
            e.generation(),
            0,
            "a fresh engine has never reindexed, so generation is 0"
        );
    }

    #[test]
    fn full_reindex_bumps_generation() {
        let cache_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(cache_dir.path());
        std::fs::write(vault_dir.path().join("a.md"), "# A\nbody").unwrap();

        // Each reindex bumps twice — once BEFORE the index loop (crash safety,
        // MAJOR 1) and once AFTER (concurrent-query gap) — so we assert strict
        // monotonic increase rather than an exact +1, which is all the memo
        // cache relies on (that the counter CHANGES across a reindex).
        assert_eq!(e.generation(), 0);
        e.reindex_all(vault_dir.path()).unwrap();
        let g1 = e.generation();
        assert!(g1 > 0, "full reindex must advance generation, got {g1}");
        e.reindex_all(vault_dir.path()).unwrap();
        assert!(
            e.generation() > g1,
            "a second full reindex advances again (even all-unchanged)"
        );
    }

    /// THE regression that matters (design §3a / §8 risk): the constantly-
    /// firing PostToolUse lex-only reindex hook changes lex results but never
    /// embeds. `last_indexed_at` is deliberately full-mode-only, so the memo
    /// cache CANNOT key on it; `generation` must bump on the lex-only path too
    /// or a stale memo entry survives an index change. This asserts exactly
    /// that: a lex-only reindex advances the counter.
    #[test]
    fn lex_only_reindex_bumps_generation() {
        let cache_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(cache_dir.path());
        std::fs::write(vault_dir.path().join("a.md"), "# A\nbody").unwrap();

        assert_eq!(e.generation(), 0);
        e.reindex_all_lex_only_with_progress(vault_dir.path(), &mut |_| {})
            .unwrap();
        let g1 = e.generation();
        assert!(
            g1 > 0,
            "lex-only reindex MUST advance generation (PostToolUse hook path), got {g1}"
        );

        // And `last_indexed_at` did NOT move — proving generation is a
        // distinct signal that tracks lex-only changes the timestamp misses.
        assert_eq!(
            e.stored_last_indexed().unwrap(),
            None,
            "lex-only must not record last_indexed_at (only generation moved)"
        );

        // A second lex-only pass over an unchanged vault still advances: the
        // counter is a coarse "something ran" signal, and over-invalidating
        // the memo cache is safe (a miss), under-invalidating is not.
        e.reindex_all_lex_only_with_progress(vault_dir.path(), &mut |_| {})
            .unwrap();
        assert!(e.generation() > g1);
    }

    /// MAJOR 1 crash-safety ordering: generation must be bumped BEFORE the
    /// index-mutation loop, so a reindex that ERRORS partway still leaves the
    /// counter advanced (new-gen memo key → miss → recompute), never old
    /// against a mutated index. Forces the paths-reindex to error via an
    /// unreadable in-vault file (`std::fs::read` → permission denied →
    /// propagates through the `?` in the paths loop). If the bump were AFTER
    /// the loop the early return would skip it and generation would stay put.
    #[test]
    #[cfg(unix)]
    fn generation_bumps_before_index_loop_even_when_reindex_errors() {
        extern "C" {
            fn geteuid() -> u32;
        }
        use std::os::unix::fs::PermissionsExt;
        // chmod 0o000 is a no-op for root, so the read wouldn't fail — skip.
        if unsafe { geteuid() } == 0 {
            return;
        }
        let cache_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(cache_dir.path());

        let doc = vault_dir.path().join("locked.md");
        std::fs::write(&doc, "# Locked\nbody").unwrap();
        std::fs::set_permissions(&doc, std::fs::Permissions::from_mode(0o000)).unwrap();

        let before = e.generation();
        // The read of the unreadable file fails and propagates via `?`, so the
        // whole reindex returns Err BEFORE reaching any post-loop bump.
        let result = e.reindex_paths(vault_dir.path(), &["locked.md".to_string()]);
        assert!(result.is_err(), "reindex of an unreadable file must error");
        assert!(
            e.generation() > before,
            "generation must be bumped before the index loop — so it advances \
             even when the reindex errors out (got {} <= {before})",
            e.generation()
        );

        // Restore perms so tempdir cleanup can remove the file.
        std::fs::set_permissions(&doc, std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[test]
    fn generation_persists_across_reopen() {
        let cache_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        std::fs::write(vault_dir.path().join("a.md"), "# A\nbody").unwrap();
        let after_reindex;
        {
            let mut e = fake_engine(cache_dir.path());
            e.reindex_all(vault_dir.path()).unwrap();
            after_reindex = e.generation();
            assert!(after_reindex > 0);
        } // drop releases the redb lock
        let e = fake_engine(cache_dir.path());
        assert_eq!(
            e.generation(),
            after_reindex,
            "generation is durable — survives an engine reopen unchanged"
        );
    }

    // ---- MAJOR 2: index-instance nonce ----

    #[test]
    fn index_nonce_is_set_at_creation_and_stable_across_reopen_and_reindex() {
        let cache_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        std::fs::write(vault_dir.path().join("a.md"), "# A\nbody").unwrap();

        let nonce = {
            let e = fake_engine(cache_dir.path());
            let n = e.index_nonce();
            assert!(!n.is_empty(), "a fresh index has a nonce");
            n
        };
        // Reopen + reindex must NOT change the nonce — same index instance.
        let mut e = fake_engine(cache_dir.path());
        assert_eq!(e.index_nonce(), nonce, "nonce stable across reopen");
        e.reindex_all(vault_dir.path()).unwrap();
        assert_eq!(e.index_nonce(), nonce, "reindex must not reset the nonce");
    }

    #[test]
    fn a_freshly_created_index_gets_a_different_nonce() {
        // Two independent fresh indexes (stand-in for a rebuild that dropped
        // and recreated engine.redb) must get distinct nonces, so their memo
        // key spaces can never overlap.
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let na = fake_engine(a.path()).index_nonce();
        let nb = fake_engine(b.path()).index_nonce();
        assert_ne!(na, nb, "distinct index instances must have distinct nonces");
    }

    #[test]
    fn doc_hash_returns_stored_hash_for_indexed_doc_and_none_otherwise() {
        let cache_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(cache_dir.path());
        std::fs::write(vault_dir.path().join("a.md"), "# A\noriginal content").unwrap();
        e.reindex_all(vault_dir.path()).unwrap();

        let h1 = e.doc_hash("a.md").expect("indexed doc has a hash");
        assert_eq!(
            Some(h1.as_str()),
            e.stored_hash("a.md").unwrap().as_deref(),
            "doc_hash must expose exactly the stored content hash (no re-hash)"
        );
        assert!(
            e.doc_hash("nope.md").is_none(),
            "unknown/unindexed doc has no hash"
        );

        // Editing the doc and reindexing changes the reported hash — the
        // property the ledger relies on to detect a doc was edited.
        std::fs::write(vault_dir.path().join("a.md"), "# A\nedited body").unwrap();
        e.reindex_all(vault_dir.path()).unwrap();
        let h2 = e.doc_hash("a.md").unwrap();
        assert_ne!(h1, h2, "an edit must change the doc_hash");
    }
}
