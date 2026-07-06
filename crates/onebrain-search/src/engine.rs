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
//! - `<cache_dir>/tantivy/` — [`crate::lex::LexIndex`] (BM25 lexical index).
//! - `<cache_dir>/vectors/` — [`crate::vector::VectorStore`] (flat mmap vector store).
//! - `<cache_dir>/engine.redb` — chunk metadata (see [`ChunkMeta`]) and the
//!   per-doc chunk-id list, both keyed by strings and serialized with
//!   `serde_json`. Neither `lex` nor `vector` stores the chunk's text or
//!   heading path, so this database is the only place [`Engine::get`] and
//!   [`Hit`] snippets can source that data from.

use std::cell::OnceCell;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::chunk::chunk_markdown;
use crate::embed::{self, Embed};
use crate::hybrid::rrf_fuse;
use crate::lex::LexIndex;
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

/// Default rerank-score gate. PROVISIONAL — like the retired `vec_floor`,
/// any unmeasured constant is a guess: the v3.4.7 Track C calibration run on
/// real vault content (golden set from the 2026-07-06 floor investigation)
/// replaces this value. Unlike `vec_floor`, a bad value here cannot silence
/// results: total rejection keeps the top [`RERANK_NO_MATCH_KEEP`] hits and
/// the CLI labels weak results instead of hiding them.
pub const DEFAULT_RERANK_MIN_SCORE: f32 = 0.30;

/// Error message returned by embedder-backed operations (`query`,
/// `vector_search`, model switch, and the lazy embedder) in a lex-only build
/// compiled without the `semantic` feature — i.e. on a platform with no ONNX
/// Runtime prebuilt. See docs/decisions/0017-platform-tiered-semantic-search.md.
pub const SEMANTIC_UNAVAILABLE: &str =
    "semantic search isn't available in this build (no ONNX runtime for this platform)";

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
    /// rerank stage scored this hit. `None` = unreranked (stage skipped,
    /// failed, or this hit sits in the fused tail beyond `candidates`).
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
    /// How many fused candidates are fed to the cross-encoder.
    pub candidates: usize,
    /// Gate: reranked hits scoring below this are dropped, subject to the
    /// never-empty floor ([`RERANK_NO_MATCH_KEEP`]).
    pub min_score: f32,
}

impl Default for RerankSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            model: rerank::reranker_registry()[0].name.to_string(),
            candidates: 30,
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
    cache_dir: PathBuf,
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
    meta: Database,
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

        let lex = LexIndex::open(&cache_dir.join("tantivy"))?;

        let vec = VectorStore::open(&cache_dir.join("vectors"), dims)?;

        let meta_path = cache_dir.join("engine.redb");
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
            }
            write_txn.commit()?;
        }

        Ok(Engine {
            lex,
            vec,
            exclude_patterns: Vec::new(),
            model_name: embed_model.to_string(),
            cache_dir: cache_dir.to_path_buf(),
            embedder,
            rerank_settings: RerankSettings::default(),
            reranker: RerankSource::Lazy(OnceCell::new()),
            rerank_error_logged: std::cell::Cell::new(false),
            chunk_corruption_logged: std::cell::Cell::new(false),
            meta,
        })
    }

    /// Path to the vector store directory, rooted at `cache_dir`.
    fn vectors_dir(&self) -> PathBuf {
        self.cache_dir.join("vectors")
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
                        Box::new(embed::new(&self.model_name, &self.cache_dir)?);
                    let _ = cell.set(e);
                    Ok(cell.get().expect("embedder was just set above").as_ref())
                }
                #[cfg(not(feature = "semantic"))]
                {
                    // Silence unused-field warnings in the lex-only build; the
                    // real embedder is what would consume these.
                    let _ = &self.model_name;
                    let _ = &self.cache_dir;
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
            if !rerank::reranker_download_status(info, &self.cache_dir).downloaded {
                return None;
            }
            match rerank::new(&self.rerank_settings.model, &self.cache_dir) {
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

    /// Whether the Tier-2 reranker is turned on per the currently installed
    /// [`RerankSettings`] (`search.reranker.enabled` in `onebrain.yml`). Lets
    /// callers distinguish an explicit `enabled: false` (no rerank attempted,
    /// no hint should be shown) from "enabled but skipped" (model not
    /// downloaded / load failure — the unreranked hint IS warranted).
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
    /// cross-encode the first `candidates` entries against `query`, sort that
    /// block by calibrated score, gate at `min_score` (never-empty: total
    /// rejection keeps the top [`RERANK_NO_MATCH_KEEP`]), append the
    /// un-reranked tail, trim to `top_k`, resolve. Skip-not-fail: no
    /// reranker → fused order with `rerank_score: None`; a rerank error
    /// falls back the same way — queries never fail because reranking did.
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

        let block_len = fused.len().min(self.rerank_settings.candidates);
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

        // Fuse wide enough for the rerank stage to see its full candidate
        // block even when the caller asks for a small top_k.
        let fuse_k = top_k.max(self.rerank_settings.candidates);
        let fused = rrf_fuse(&lex_hits, &vec_hits, RRF_K, fuse_k);
        self.apply_rerank(text, fused, top_k)
    }

    /// Vector-only semantic search (no lex/RRF fusion): embed `text`, take
    /// the top nearest chunks by cosine similarity, then pass them through
    /// the Tier-2 rerank stage ([`Self::apply_rerank`]). Used by the CLI's
    /// `search vsearch` verb.
    pub fn vector_search(&self, text: &str, top_k: usize) -> Result<Vec<Hit>> {
        let query_vec = self.embedder()?.embed_query(text)?;
        let fetch_k = top_k.max(self.rerank_settings.candidates);
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
                .map(|info| rerank::reranker_download_status(info, &self.cache_dir).downloaded)
                .unwrap_or(true);
            if should_fetch_reranker(self.rerank_settings.enabled, downloaded) {
                progress(ReindexProgress::LoadingReranker);
                // Resolve the lazy reranker once so the download/construction
                // happens now (hf-hub prints its own progress), not on the
                // next query. `reranker()` is skip-not-fail by design — a
                // download error never fails the reindex.
                self.reranker();
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
        Engine::open_with_embedder(dir, "fake-model", Box::new(FakeEmbedder { dims: 16 })).unwrap()
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
        // no reranker model downloaded, so `should_fetch_reranker` is true.
        // The fake embedder engine still resolves the LAZY reranker path
        // (cheaply, to None) — assert LoadingReranker fires exactly once,
        // after every Indexing event.
        let cache_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(cache_dir.path());

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
    fn rerank_tail_beyond_candidates_is_appended_unreranked() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = fake_engine(dir.path());
        for i in 0..4 {
            e.index_doc(&format!("t{i}.md"), &format!("zeta unique{i}"))
                .unwrap();
        }
        e.set_rerank_settings(RerankSettings {
            candidates: 2,
            min_score: 0.0,
            ..Default::default()
        });
        e.set_reranker_for_tests(Box::new(FakeReranker));
        let hits = e.query("zeta", 10).unwrap();
        assert_eq!(hits.len(), 4);
        assert!(
            hits[0].rerank_score.is_some() && hits[1].rerank_score.is_some(),
            "first `candidates` hits form the reranked block"
        );
        assert!(
            hits[2].rerank_score.is_none() && hits[3].rerank_score.is_none(),
            "fused tail beyond `candidates` stays unreranked, appended after"
        );
        assert!(hits[0].rerank_score.unwrap() >= hits[1].rerank_score.unwrap());
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
        // shrinks below top_k. (The fused tail beyond `candidates` is a
        // separate mechanism, covered by the test above.) Track B confidence
        // bands depend on this exact semantic.
        assert_eq!(hits.len(), 2, "gate-dropped candidates must not reappear");
        assert!(hits.iter().all(|h| h.doc_path.starts_with("m")));
        assert!(hits.iter().all(|h| h.rerank_score == Some(0.9)));
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
}
