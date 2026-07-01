//! Ties `chunk` + `embed` + `vector` + `lex` + `hybrid` together into a single
//! synchronous search engine: index a document, remove a document, and run a
//! hybrid (lex + vector, RRF-fused) query.
//!
//! ## On-disk layout (under `cache_dir`)
//! - `<cache_dir>/tantivy/` — [`crate::lex::LexIndex`] (BM25 lexical index).
//! - `<cache_dir>/vectors/` — [`crate::vector::VectorStore`] (flat mmap vector store).
//! - `<cache_dir>/engine.redb` — chunk metadata (see [`ChunkMeta`]) and the
//!   per-doc chunk-id list, both keyed by strings and serialized with
//!   `serde_json`. Neither `lex` nor `vector` stores the chunk's text or
//!   heading path, so this database is the only place [`Engine::get`] and
//!   [`Hit`] snippets can source that data from.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::chunk::chunk_markdown;
use crate::embed::{self, Embedder};
use crate::hybrid::rrf_fuse;
use crate::lex::LexIndex;
use crate::vector::VectorStore;

const CHUNK_META: TableDefinition<&str, &str> = TableDefinition::new("chunk_meta");
const DOC_CHUNKS: TableDefinition<&str, &str> = TableDefinition::new("doc_chunks");
const DOC_HASHES: TableDefinition<&str, &str> = TableDefinition::new("doc_hashes");

const CHUNK_MAX_TOKENS: usize = 512;
const CHUNK_OVERLAP_TOKENS: usize = 64;
const LEX_TOP_K: usize = 50;
const VEC_TOP_K: usize = 50;
const RRF_K: f64 = 60.0;
const SNIPPET_MAX_CHARS: usize = 200;

/// Outcome counts of a [`Engine::reindex_paths`] / [`Engine::reindex_all`] run.
#[derive(Debug, Default, PartialEq)]
pub struct ReindexStats {
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    pub unchanged: usize,
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

/// Recursively collect every `*.md` file under `root` (hand-rolled
/// stack-based walk — no new crate dep).
fn walk_markdown_files(root: &Path) -> Result<Vec<PathBuf>> {
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
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "md") {
                out.push(path);
            }
        }
    }
    Ok(out)
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

/// The assembled search engine: lexical index, vector store, embedder, and
/// a `redb` metadata database, all rooted at one `cache_dir`.
pub struct Engine {
    lex: LexIndex,
    vec: VectorStore,
    embedder: Embedder,
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
    /// the embedding model (dims resolved via [`embed::model_dims`]).
    pub fn open(cache_dir: &Path, embed_model: &str) -> Result<Self> {
        std::fs::create_dir_all(cache_dir)
            .with_context(|| format!("creating cache dir {}", cache_dir.display()))?;

        let lex = LexIndex::open(&cache_dir.join("tantivy"))?;

        let dims = embed::model_dims(embed_model);
        let vec = VectorStore::open(&cache_dir.join("vectors"), dims)?;

        let embedder = embed::new(embed_model, cache_dir)?;

        let meta_path = cache_dir.join("engine.redb");
        let meta = Database::create(&meta_path)
            .with_context(|| format!("opening redb database {}", meta_path.display()))?;
        {
            let write_txn = meta.begin_write()?;
            write_txn.open_table(CHUNK_META)?;
            write_txn.open_table(DOC_CHUNKS)?;
            write_txn.open_table(DOC_HASHES)?;
            write_txn.commit()?;
        }

        Ok(Engine {
            lex,
            vec,
            embedder,
            meta,
        })
    }

    /// Chunk `content`, index into lex + embed + vector, and record chunk
    /// meta. Returns the number of chunks indexed.
    pub fn index_doc(&mut self, doc_path: &str, content: &str) -> Result<usize> {
        let chunks = chunk_markdown(doc_path, content, CHUNK_MAX_TOKENS, CHUNK_OVERLAP_TOKENS);

        let mut chunk_ids: Vec<String> = Vec::with_capacity(chunks.len());
        let write_txn = self.meta.begin_write()?;
        {
            let mut chunk_meta = write_txn.open_table(CHUNK_META)?;
            for chunk in &chunks {
                self.lex.add(chunk)?;

                let vectors = self.embedder.embed(std::slice::from_ref(&chunk.text))?;
                self.vec.add(&chunk.chunk_id, &vectors[0])?;

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

    /// Hybrid search: lex + vec (top ~50 each) fused via RRF, resolved to
    /// `top_k` [`Hit`]s. Fused ids whose meta is missing are skipped.
    pub fn query(&self, text: &str, top_k: usize) -> Result<Vec<Hit>> {
        let query_vec = self.embedder.embed(&[text.to_string()])?;
        let vec_hits = self.vec.search(&query_vec[0], VEC_TOP_K);
        let lex_hits = self.lex.search(text, LEX_TOP_K)?;

        let fused = rrf_fuse(&lex_hits, &vec_hits, RRF_K, top_k);

        let read_txn = self.meta.begin_read()?;
        let chunk_meta = read_txn.open_table(CHUNK_META)?;

        let mut hits = Vec::with_capacity(fused.len());
        for (chunk_id, score) in fused {
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
    fn reindex_existing_doc(
        &mut self,
        doc_path: &str,
        abs_path: &Path,
        stats: &mut ReindexStats,
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
                self.index_doc(doc_path, &content)?;
                self.store_hash(doc_path, &current_hash)?;
                stats.added += 1;
            }
            HashDiff::Updated => {
                let content = String::from_utf8_lossy(&bytes).into_owned();
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
        let mut stats = ReindexStats::default();
        for doc_path in doc_paths {
            let abs_path = vault_root.join(doc_path);
            if abs_path.is_file() {
                self.reindex_existing_doc(doc_path, &abs_path, &mut stats)?;
            } else if self.stored_hash(doc_path)?.is_some() {
                self.remove_doc(doc_path)?;
                self.drop_hash(doc_path)?;
                stats.removed += 1;
            }
            // Neither on disk nor indexed: ignore.
        }
        Ok(stats)
    }

    /// Reindex the whole vault: walk `vault_root` for `*.md` files
    /// (recursively), reindex each, and remove any previously-indexed doc
    /// whose file no longer exists.
    pub fn reindex_all(&mut self, vault_root: &Path) -> Result<ReindexStats> {
        let files = walk_markdown_files(vault_root)?;
        let doc_paths: Vec<String> = files
            .iter()
            .filter_map(|f| vault_relative_path(vault_root, f))
            .collect();

        let mut stats = ReindexStats::default();
        for (doc_path, abs_path) in doc_paths.iter().zip(files.iter()) {
            self.reindex_existing_doc(doc_path, abs_path, &mut stats)?;
        }

        // Sweep: any stored hash whose doc_path wasn't just seen on disk
        // means the file is gone.
        let stale: Vec<String> = {
            let read_txn = self.meta.begin_read()?;
            let doc_hashes = read_txn.open_table(DOC_HASHES)?;
            doc_hashes
                .iter()?
                .filter_map(|entry| entry.ok())
                .map(|(k, _)| k.value().to_string())
                .filter(|k| !doc_paths.contains(k))
                .collect()
        };
        for doc_path in stale {
            self.remove_doc(&doc_path)?;
            self.drop_hash(&doc_path)?;
            stats.removed += 1;
        }

        Ok(stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            }
        );
        assert!(e.get("a.md").is_err());
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
