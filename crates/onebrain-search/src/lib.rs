//! `onebrain-search` — OneBrain's native, in-binary vault search engine.
//!
//! Replaces the former external `qmd` (Node) dependency with a self-contained
//! Rust stack: keyword (BM25), semantic (embeddings), and hybrid search over a
//! vault of markdown notes, with no Node/Python and no separate install. See
//! `docs/decisions/0012-native-search-replace-qmd.md` for the rationale.
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
//! Per collection, under `~/.cache/onebrain/search/<collection>/`: a `tantivy/`
//! index directory, a `vectors.bin` flat vector file, and a `meta.redb`
//! metadata database. All outside the vault.

pub mod chunk;
pub mod embed;
pub mod engine;
pub mod error;
pub mod hybrid;
pub mod lex;
pub mod vector;
