//! `onebrain-search` — OneBrain's native, in-binary vault search engine.
//!
//! Replaces the former external `qmd` (Node) dependency with a self-contained
//! Rust stack: keyword (BM25), semantic (embeddings), and hybrid search over a
//! vault of markdown notes, with no Node/Python and no separate install. See
//! `docs/decisions/0012-native-search-replace-qmd.md` for the rationale.
//!
//! # Indexing scope
//!
//! The engine indexes **Markdown (`*.md`) files only**. The vault walk
//! ([`engine::reindex_all_with_progress`]) collects nothing but `*.md` files;
//! every other file type in the vault (images, PDFs, code, …) is never read,
//! chunked, embedded, or stored. Hidden directories and `node_modules` are
//! skipped, plus any `search.exclude` patterns from `onebrain.yml`.
//!
//! # Architecture
//!
//! One sync engine, assembled from focused modules:
//!
//! ```text
//! markdown ──▶ [chunk] ──┬─▶ [lex]  tantivy BM25 (Thai-aware tokenizer) ──┐
//!                        └─▶ [embed] fastembed vectors ─▶ [vector] mmap store ─┤
//!                                                                              ▼
//!                                          query ─▶ lex + vec ─▶ [hybrid] RRF ─▶ top-k
//! ```
//!
//! - [`chunk`] — split a note into heading-aware, size-bounded chunks, each
//!   carrying its heading path.
//! - [`embed`] — wrap `fastembed` to produce L2-normalized embedding vectors;
//!   holds the swappable-model registry (default `multilingual-e5-small`).
//! - [`vector`] — flat, memory-mapped `f32` store with exact cosine (dot) top-k
//!   search via `simsimd`, plus `redb` metadata (chunk↔row, tombstones,
//!   free-list).
//! - [`lex`] — `tantivy` BM25 index with a Thai-aware tokenizer. Thai currently
//!   uses a character-bigram fallback (the `nlpo3` word dictionary is not
//!   bundled); the vector side carries Thai semantics in the meantime.
//! - [`hybrid`] — Reciprocal Rank Fusion (RRF) of the lex and vector result
//!   lists.
//! - [`engine`] — ties the pieces together: index a doc, remove a doc, query.
//!
//! # Frontends
//!
//! This crate is the engine only. It is driven by the `onebrain-cli` `search`
//! command group and (from v3.4.1) OneBrain's own MCP server. The engine core
//! is synchronous; async lives only at the MCP boundary in the CLI crate.
//!
//! # Storage
//!
//! Per collection, under the platform data dir
//! (`~/Library/Application Support/onebrain/search/<collection>/` on macOS ·
//! `$XDG_DATA_HOME/onebrain/search/<collection>/` on Linux ·
//! `%APPDATA%\onebrain\search\<collection>\` on Windows): a `tantivy/` index
//! directory, a `vectors/` flat vector store, and an `engine.redb` metadata
//! database. Downloaded embedding models live alongside them in `models--*`
//! dirs. All outside the vault, and outside the OS-purgeable cache dir (moved
//! there in v3.4.5, issue #114). The CLI owns the concrete path
//! (`onebrain_cli::commands::search_common::search_cache_root`).

pub mod chunk;
pub mod embed;
pub mod engine;
pub mod error;
pub mod hybrid;
pub mod layout;
pub mod lex;
pub mod rerank;
pub mod vector;

pub use layout::{CacheLayoutState, CollectionLayout};
