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
use crate::vector::VectorStore;

const CHUNK_META: TableDefinition<&str, &str> = TableDefinition::new("chunk_meta");
const DOC_CHUNKS: TableDefinition<&str, &str> = TableDefinition::new("doc_chunks");
const DOC_HASHES: TableDefinition<&str, &str> = TableDefinition::new("doc_hashes");
const ENGINE_HEADER: TableDefinition<&str, &str> = TableDefinition::new("engine_header");

const ACTIVE_MODEL_KEY: &str = "active_model";
const LAST_INDEXED_KEY: &str = "last_indexed_at";

const CHUNK_MAX_TOKENS: usize = 512;
const CHUNK_OVERLAP_TOKENS: usize = 64;
const LEX_TOP_K: usize = 50;
const VEC_TOP_K: usize = 50;
const RRF_K: f64 = 60.0;
const SNIPPET_MAX_CHARS: usize = 200;

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

/// Drop vector hits whose cosine similarity is below the model's measured
/// confidence floor (see `ModelInfo::vec_floor`). Without this, a query
/// about something the vault doesn't contain still surfaces its top-k
/// nearest neighbors — pure noise with authoritative-looking ranks.
fn drop_below_floor(hits: Vec<(String, f32)>, floor: Option<f32>) -> Vec<(String, f32)> {
    match floor {
        Some(f) => hits.into_iter().filter(|(_, s)| *s >= f).collect(),
        None => hits,
    }
}

/// A single fused, resolved search hit.
pub struct Hit {
    pub chunk_id: String,
    pub doc_path: String,
    pub heading_path: String,
    pub score: f64,
    pub snippet: String,
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
    meta: Database,
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

    /// The active model's vector-confidence floor from the registry
    /// (`None` for unknown/test models and models without a measured floor).
    fn vec_floor(&self) -> Option<f32> {
        embed::model_registry()
            .iter()
            .find(|m| m.name == self.model_name)
            .and_then(|m| m.vec_floor)
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
                let e: Box<dyn Embed> = Box::new(embed::new(&self.model_name, &self.cache_dir)?);
                let _ = cell.set(e);
                Ok(cell.get().expect("embedder was just set above").as_ref())
            }
        }
    }

    /// Chunk `content`, index into lex + embed + vector, and record chunk
    /// meta. Returns the number of chunks indexed.
    pub fn index_doc(&mut self, doc_path: &str, content: &str) -> Result<usize> {
        let chunks = chunk_markdown(doc_path, content, CHUNK_MAX_TOKENS, CHUNK_OVERLAP_TOKENS);

        // Batch-embed ALL chunk texts in one call — `Embedder::embed` takes a
        // slice and batches internally, so one call per doc is far cheaper
        // than one call per chunk. The returned vectors are index-aligned with
        // `chunks`.
        let texts: Vec<String> = chunks.iter().map(|c| c.text.clone()).collect();
        let vectors = if texts.is_empty() {
            Vec::new()
        } else {
            self.embedder()?.embed_passages(&texts)?
        };

        let mut chunk_ids: Vec<String> = Vec::with_capacity(chunks.len());
        let write_txn = self.meta.begin_write()?;
        {
            let mut chunk_meta = write_txn.open_table(CHUNK_META)?;
            for (i, chunk) in chunks.iter().enumerate() {
                self.lex.add(chunk)?;

                self.vec.add(&chunk.chunk_id, &vectors[i])?;

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

    /// Resolve a list of `(chunk_id, score)` pairs (already ranked, already
    /// truncated to the caller's desired top-k) into full [`Hit`]s by
    /// looking up each chunk's stored meta. Ids whose meta is missing are
    /// skipped. Shared by [`Self::query`] and [`Self::vector_search`].
    fn resolve_hits(&self, ranked: Vec<(String, f64)>) -> Result<Vec<Hit>> {
        let read_txn = self.meta.begin_read()?;
        let chunk_meta = read_txn.open_table(CHUNK_META)?;

        let mut hits = Vec::with_capacity(ranked.len());
        for (chunk_id, score) in ranked {
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
            });
        }
        Ok(hits)
    }

    /// Hybrid search: lex + vec (top ~50 each) fused via RRF, resolved to
    /// `top_k` [`Hit`]s. Fused ids whose meta is missing are skipped.
    pub fn query(&self, text: &str, top_k: usize) -> Result<Vec<Hit>> {
        let query_vec = self.embedder()?.embed_query(text)?;
        let vec_hits = drop_below_floor(self.vec.search(&query_vec, VEC_TOP_K), self.vec_floor());
        let lex_hits = self.lex.search(text, LEX_TOP_K)?;

        let fused = rrf_fuse(&lex_hits, &vec_hits, RRF_K, top_k);
        self.resolve_hits(fused)
    }

    /// Vector-only semantic search (no lex/RRF fusion): embed `text` and
    /// return the top-k nearest chunks by cosine similarity, resolved to
    /// full [`Hit`]s. Used by the CLI's `search vsearch` verb.
    pub fn vector_search(&self, text: &str, top_k: usize) -> Result<Vec<Hit>> {
        let query_vec = self.embedder()?.embed_query(text)?;
        let vec_hits = drop_below_floor(self.vec.search(&query_vec, top_k), self.vec_floor());
        let ranked: Vec<(String, f64)> = vec_hits
            .into_iter()
            .map(|(id, score)| (id, score as f64))
            .collect();
        self.resolve_hits(ranked)
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

    /// Drop `doc_path`'s stored content hash, if any.
    fn drop_hash(&mut self, doc_path: &str) -> Result<()> {
        let write_txn = self.meta.begin_write()?;
        {
            let mut doc_hashes = write_txn.open_table(DOC_HASHES)?;
            doc_hashes.remove(doc_path)?;
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
    /// don't embed, so they never trigger it.
    fn reindex_existing_doc(
        &mut self,
        doc_path: &str,
        abs_path: &Path,
        stats: &mut ReindexStats,
        on_first_embed: &mut dyn FnMut(),
    ) -> Result<()> {
        let bytes =
            std::fs::read(abs_path).with_context(|| format!("reading {}", abs_path.display()))?;
        let current_hash = hash_bytes(&bytes);
        let stored = self.stored_hash(doc_path)?;

        match diff_hash(stored.as_deref(), &current_hash) {
            HashDiff::Unchanged => {
                stats.unchanged += 1;
            }
            HashDiff::Added => {
                let content = String::from_utf8_lossy(&bytes).into_owned();
                on_first_embed();
                self.index_doc(doc_path, &content)?;
                self.store_hash(doc_path, &current_hash)?;
                stats.added += 1;
            }
            HashDiff::Updated => {
                let content = String::from_utf8_lossy(&bytes).into_owned();
                on_first_embed();
                self.remove_doc(doc_path)?;
                self.index_doc(doc_path, &content)?;
                self.store_hash(doc_path, &current_hash)?;
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
        let total = doc_paths.len();
        progress(ReindexProgress::Walked { total });
        let mut model_announced = false;
        let mut stats = ReindexStats::default();
        for (i, doc_path) in doc_paths.iter().enumerate() {
            let abs_path = vault_root.join(doc_path);
            if abs_path.is_file() {
                let mut on_first_embed = || {
                    if !model_announced {
                        model_announced = true;
                        progress(ReindexProgress::LoadingModel);
                    }
                };
                self.reindex_existing_doc(doc_path, &abs_path, &mut stats, &mut on_first_embed)?;
            } else if self.stored_hash(doc_path)?.is_some() {
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
        self.record_last_indexed(now_epoch_secs())?;
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
                self.reindex_existing_doc(doc_path, abs_path, &mut stats, &mut on_first_embed)
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
        let seen: std::collections::HashSet<&String> = doc_paths.iter().collect();
        let stale: Vec<String> = {
            let read_txn = self.meta.begin_read()?;
            let doc_hashes = read_txn.open_table(DOC_HASHES)?;
            doc_hashes
                .iter()?
                .filter_map(|entry| entry.ok())
                .map(|(k, _)| k.value().to_string())
                .filter(|k| !seen.contains(k))
                .collect()
        };
        for doc_path in stale {
            self.remove_doc(&doc_path)?;
            self.drop_hash(&doc_path)?;
            stats.removed += 1;
        }

        self.record_last_indexed(now_epoch_secs())?;
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

        let files = walk_markdown_files(vault_root, &self.exclude_patterns)?;
        let mut pending_new = 0usize;
        let mut pending_changed = 0usize;
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for abs_path in &files {
            let Some(doc_path) = vault_relative_path(vault_root, abs_path) else {
                continue;
            };
            let bytes = match std::fs::read(abs_path) {
                Ok(b) => b,
                // Unreadable file: skip it (a reindex would count it as
                // `failed`, not as drift). Keep status read-only + resilient.
                Err(_) => continue,
            };
            let current_hash = hash_bytes(&bytes);
            match diff_hash(stored.get(&doc_path).map(String::as_str), &current_hash) {
                HashDiff::Added => pending_new += 1,
                HashDiff::Updated => pending_changed += 1,
                HashDiff::Unchanged => {}
            }
            seen.insert(doc_path);
        }

        // Indexed docs whose file is gone from disk (would be removed).
        let pending_removed = stored.keys().filter(|k| !seen.contains(*k)).count();

        Ok(IndexStatus {
            doc_count: stored.len(),
            last_indexed_at: self.stored_last_indexed()?,
            pending_new,
            pending_changed,
            pending_removed,
        })
    }
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

    #[test]
    fn drop_below_floor_filters_only_with_floor() {
        let hits = vec![("a".to_string(), 0.88f32), ("b".to_string(), 0.84)];
        let kept = drop_below_floor(hits.clone(), Some(0.85));
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].0, "a");
        assert_eq!(
            drop_below_floor(hits, None).len(),
            2,
            "no floor → untouched"
        );
    }

    #[test]
    fn e5_family_has_vec_floor_bge_does_not() {
        for m in embed::model_registry() {
            if m.name.starts_with("multilingual-e5") {
                assert_eq!(m.vec_floor, Some(0.85), "{}", m.name);
            }
        }
        let bge = embed::model_registry()
            .iter()
            .find(|m| m.name == "bge-m3")
            .unwrap();
        assert!(bge.vec_floor.is_none());
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
}
