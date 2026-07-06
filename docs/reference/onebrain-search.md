# onebrain-search

> **Process view** — for the end-to-end system reference (storage map, the reindex/embed pipeline incl. the auto-hooks, every search surface's read path + degradation matrix), see [`docs/architecture/search.md`](../architecture/search.md). This file stays the crate-internals, module-by-module reference.

## Purpose & dependencies
`onebrain-search` is OneBrain's native, in-binary vault search engine — the v3.4.0 replacement for the external `qmd` (Node) dependency. It owns the full retrieval stack over a vault of markdown notes: heading-aware chunking, BM25 lexical search (`tantivy`) with a script-aware tokenizer for no-space scripts (Thai, Lao, Myanmar, Khmer, CJK), local ONNX embeddings (`fastembed`), a flat memory-mapped vector store with exact cosine top-k, Reciprocal Rank Fusion of the two rankings, a Tier-2 cross-encoder rerank stage over the fused/retrieved candidates ([ADR 0025](../decisions/0025-tier2-cross-encoder-reranker.md)), and the `Engine` that ties them together (index/remove/query/status/reindex/rebuild). It depends on **nothing in-workspace** — only external crates: `tantivy` 0.26 (BM25; feature-trimmed to `mmap`, `lz4-compression`, `stopwords`, `stemmer` — default features are disabled to avoid the C zstd build dep pulled in by the non-default `zstd-compression` feature, per the Cargo.toml comment), `fastembed` 5.17 (ONNX-backed local embedding + reranking models), `simsimd` 6.5 (SIMD dot-product similarity), `memmap2` (mmap access to the vector file), `redb` 4.1 (embedded KV metadata), `hf-hub` (reranker model download, same HF hub cache convention as the embedder), plus `serde`/`serde_json`, `anyhow`, `sha2` (content hashing + reranker model integrity verification). `nlpo3` (Thai word segmentation) is deliberately **not** a dependency yet — the published crate excludes its word dictionary, so v3.4.0 ships a dictionary-free character-bigram fallback in `src/lex.rs` (Phase 2 Thai upgrade re-adds it). The only downstream crate is **`onebrain-cli`**: the `search` verb group (`search_query`, `search_reindex`, `search_status`, `search_get`, `search_model`, `search_model_tui`), the `search_common.rs` collection→cache-dir plumbing, and `doctor`'s native-search index check. `onebrain-fs` does not depend on it.

## Module map
```
src/
├── lib.rs       Crate root · module declarations + crate-level architecture/scope/storage docs (no re-exports)
├── chunk.rs     Heading-aware + size-split markdown chunker → Chunk (heading_path "A > B > C")
├── embed.rs     Embed trait · fastembed Embedder · 6-entry ModelInfo registry · download-status helpers
├── lex.rs       tantivy BM25 index (LexIndex) · script-aware tokenizer · char-bigrams for no-space scripts
├── vector.rs    Flat mmap f32 vector store (VectorStore) · redb metadata · tombstones/free-list · compact
├── hybrid.rs    rrf_fuse — Reciprocal Rank Fusion of the lex and vector rankings
├── rerank.rs    Rerank trait · fastembed cross-encoder Reranker · 1-entry RerankerInfo registry · sha256 verify · sigmoid calibration
└── engine.rs    Engine — open/index_doc/remove_doc/query/vector_search/get/status/reindex/rebuild (query/vector_search now run the Tier-2 rerank stage)
```

## On-disk layout (per collection)
Everything lives outside the vault, under the persistent data dir — `<data_dir>/onebrain/search/<collection>/` (macOS `~/Library/Application Support/onebrain/search/<collection>/` · Linux `$XDG_DATA_HOME/onebrain/search/<collection>/` · Windows `%APPDATA%\onebrain\search\<collection>\`), resolved by the CLI's `search_common::collection_cache_dir` from the `search.collection` config value. (Relocated out of the OS-purgeable cache dir in v3.4.5 — issue #114, [ADR 0021](../decisions/0021-search-state-persistent-data-dir.md).)
- `tantivy/` — the BM25 lexical index (`src/lex.rs`).
- `vectors/` — the flat vector store (`src/vector.rs`): `vectors.bin` (packed little-endian `f32`, row-major, stride = `dims * 4` bytes) + `meta.redb` (chunk↔row mapping, tombstones, free-list, header).
- `engine.redb` — engine metadata (`src/engine.rs`): chunk text/heading meta, per-doc chunk lists, doc content hashes, active model + last-indexed header.
- `models--*` dirs — embedding models downloaded by `fastembed`/`hf-hub` (e.g. `models--intfloat--multilingual-e5-small`), named via `ModelInfo::cache_dir_name`.
- `reindex-progress.json` — transient live marker for an in-flight reindex. Written/removed by the CLI (`reindex_progress_path` in `crates/onebrain-cli/src/commands/search_common.rs`), **not** by this crate — `search status` reads it to report a running reindex.

Model download behavior: only the paths that actually embed — `index_doc`, `query`, `vector_search`, `rebuild` (via the lazy `Engine::embedder`) — construct the real `fastembed` embedder and can trigger a first-time model download. `Engine::open`, `status`, `get`, and the CLI's model `list`/download-status views never download.

## Feature flags
`semantic` (**default ON**) gates the `fastembed`/ONNX embedding path — the real `embed::Embedder`, `embed::new`/`new_quiet`, `resolve_model`, and the `Engine::embedder` lazy real-embedder init. It's an optional-dependency feature: `fastembed` is only compiled when `semantic` is enabled, and with `default-features = false` + the `hf-hub-rustls-tls`/`ort-download-binaries-rustls-tls` features (rustls, not openssl; no `image-models`). Building `--no-default-features` yields a **lex-only** crate: chunking, the lex/BM25 index, the vector store, the `Embed` trait + fake embedders, and the whole `ModelInfo` registry stay unconditional (they're pure/portable), so keyword search and the full index lifecycle work everywhere. Without `semantic`, `Engine::index_doc` lex-indexes + records meta/hash but skips embedding (the vector store is left empty via `embed_passages_if_available` → `None`), and constructing a real embedder returns `engine::SEMANTIC_UNAVAILABLE` ("semantic search isn't available in this build (no ONNX runtime for this platform)"). This is the seam for platform-tiered release builds — see [ADR 0017](../decisions/0017-platform-tiered-semantic-search.md) for the rationale + tier table, and the [platform-support matrix](../platform-support.md) for the per-target ✅/❌ breakdown (which targets ship lex-only). `onebrain-cli` has a matching `semantic` feature (default ON) that forwards to `onebrain-search/semantic`.

## `src/lib.rs`
Crate root. Declares the seven modules (`chunk`, `embed`, `engine`, `hybrid`, `lex`, `rerank`, `vector`) — **no re-exports**: consumers use full paths (`onebrain_search::engine::Engine`, `onebrain_search::embed::model_registry`, `onebrain_search::rerank::reranker_registry`, …). The crate-level doc comment carries the architecture diagram (markdown → chunk → lex + embed/vector → RRF → rerank), the `.md`-only indexing scope, the per-collection storage layout, and the frontends note (driven by the `onebrain-cli` `search` command group; OneBrain's own MCP server from v3.4.1 — the engine core stays synchronous, async lives only at the MCP boundary in the CLI crate). (An eighth module, `error.rs`, predates this rerank work (v3.4.6) and is not covered by this map — a pre-existing gap, not introduced here.)
**Connections** — module declarations only; called by: `onebrain-cli` (entry surface).

## `src/chunk.rs`
Heading-aware + size-split markdown chunker. Splits a document along ATX heading boundaries (`#`…`######`, space-after-hash required), tracking the active heading stack so each chunk carries its full `heading_path` (`"A > B > C"`); any section body exceeding `max_tokens` is further sliced into overlapping windows. Token counting is a whitespace-word approximation (`split_whitespace().count()`), not a real tokenizer. The size parameters are owned by the caller — `engine.rs` passes `CHUNK_MAX_TOKENS = 512` / `CHUNK_OVERLAP_TOKENS = 64`.
**Key types**
- `Chunk` — `{ chunk_id, doc_path, heading_path, chunk_index, text }`; `chunk_id` is `"<doc_path>#<chunk_index>"` (e.g. `n.md#0`).

**Key functions**
- `chunk_markdown(doc_path, content, max_tokens, overlap_tokens) -> Vec<Chunk>` — flush the accumulated section text on every heading (popping stack entries with level ≥ the new heading's before pushing it), then size-split each section.
- (private: `parse_heading(line) -> Option<(usize, &str)>` — ATX detection; `split_body_into_windows(body, max_tokens, overlap_tokens)` — the last `overlap_tokens` words of window k reappear as the first words of window k+1, stride = `max - overlap`.)

**Connections** — calls: nothing (pure); called by: `engine::index_doc`.
**Tests** — `#[cfg(test)]` covers heading-path nesting, size-split overlap (last-64-words-of-k = first-64-of-k+1), empty input, and heading-less prose (empty `heading_path`).

## `src/embed.rs`
Wraps `fastembed` to turn chunk texts into **L2-normalized** embedding vectors, and owns the swappable-model registry. Normalization is unconditional (even for models that already emit near-unit vectors) so the vector store can assume unit length and use a plain dot product as cosine.
**Key types**
- `trait Embed` — the embedding seam: `embed(texts)`, `dims()`, plus `embed_passages` / `embed_query` with pass-through defaults. `Embedder` is the real implementation; engine tests inject a deterministic in-memory fake (`FakeEmbedder`, defined in `src/engine.rs`'s `#[cfg(test)]` module) via `Engine::open_with_embedder` so index/query/rebuild logic runs without a multi-GB download.
- `Embedder` — `Mutex<TextEmbedding>` + `dims`, `model_name`, and the model's `query_prefix`/`passage_prefix`. `embed_passages` prepends the passage prefix, `embed_query` the query prefix (stored chunk text stays raw — prefixes apply at embed time only).
- `ModelInfo` — one registry entry: `name` (config-facing `search.embed_model` value), `dims`, `approx_size`/`approx_bytes` (the denominator for download-progress %), `context` (max input tokens), `thai_miracl` (`Option` — `None` = unverified for Thai), `note`, `query_prefix`/`passage_prefix`, `hf_repo`.
- `ModelDownloadStatus` — `{ downloaded, disk_size: Option<u64>, path }`, computed per model from a collection cache dir.

**The registry** (`model_registry()` — display order, smallest/default first; the single source of truth for model names):

| name | dims | context | approx size | note | prefixes (query · passage) |
|---|---|---|---|---|---|
| `multilingual-e5-small` | 384 | 512 | ~470 MB | default · small + fast | `query: ` · `passage: ` |
| `multilingual-e5-base` | 768 | 512 | ~1.1 GB | larger · better recall | `query: ` · `passage: ` |
| `multilingual-e5-large` | 1024 | 512 | ~2.1 GB | high accuracy | `query: ` · `passage: ` |
| `bge-m3` | 1024 | 8192 | ~2.2 GB | best Thai/accuracy · fp32 | (none) |
| `embeddinggemma-300m-q` | 768 | 2048 | ~310 MB | small · int8 · Thai unverified | `task: search result \| query: ` · `title: none \| text: ` |
| `embeddinggemma-300m-q4` | 768 | 2048 | ~200 MB | smallest · 4-bit · Thai unverified | `task: search result \| query: ` · `title: none \| text: ` |

The e5 family was trained with instruction prefixes (omitting them measurably degrades retrieval). Vector-hit trimming is a recall-first relative cutoff (`keep_top_cluster`), not a per-model absolute floor — see [ADR 0024](../decisions/0024-vector-confidence-recall-first.md). `keep_top_cluster` is where precision handling stops for the vector leg itself; the actual precision gate is the Tier-2 rerank stage that runs on the trimmed candidates afterward — see `src/rerank.rs` below and [ADR 0025](../decisions/0025-tier2-cross-encoder-reranker.md). The two embeddinggemma variants **share one HF repo** (`onnx-community/embeddinggemma-300m-ONNX`) and therefore one `models--onnx-community--embeddinggemma-300m-ONNX` cache dir — `model_download_status` can't tell them apart, so downloading either marks both as downloaded and the reported disk size is the dir total.

**Key functions**
- `new(model_name, cache_dir) -> Result<Embedder>` / `new_quiet(..)` — load the fastembed model, caching downloads under `cache_dir` (`InitOptions::with_cache_dir` + `with_show_download_progress`). `new` prints fastembed's stdout download bar on a first-time download; `new_quiet` disables it (the interactive model TUI runs the terminal in raw mode and draws its own in-table progress — a stray stdout print would corrupt it).
- `resolve_model(model_name) -> Result<EmbeddingModel>` (private) — registry name → fastembed enum (`BGEM3`, `MultilingualE5Small/Base/Large`, `EmbeddingGemma300MQ`, `EmbeddingGemma300MQ4`); a registry entry with no mapping is reported as a bug, unknown names get the supported-name list.
- `is_supported_model(name) -> bool` / `model_dims(name) -> usize` — both derived from `model_registry` (never out of sync); `model_dims` returns `0` for unknown names (strict validation is `new`'s job).
- `ModelInfo::cache_dir_name() -> String` — `models--{org}--{repo}` (hf-hub maps `org/repo` by replacing `/` with `--`).
- `model_download_status(info, cache_dir) -> ModelDownloadStatus` — pure `std::fs`: dir-exists check + size sum; never downloads, never opens the engine.
- `dir_size_bytes(root) -> u64` — hand-rolled stack walk summing regular-file sizes (no `du`, no subprocess; symlinks not followed). Public so the CLI's TUI can poll a model dir's growth as download progress.
- (private: `l2_normalize` — no-op on a zero vector to avoid dividing by zero.)

**Connections** — calls: `fastembed::{TextEmbedding, InitOptions, EmbeddingModel}`; called by: `engine::embedder` (lazy construction), and directly by onebrain-cli's `search model` verbs / model TUI (registry, download status, `new_quiet`).
**Tests** — registry invariants (exactly six models, prefixes, dims↔registry sync, gemma repo/cache-dir conflation, every entry resolvable), cache-dir naming, download-status with/without dir, plus a network-gated real-embed normalization test (`ONEBRAIN_TEST_EMBED`).

## `src/lex.rs`
Tantivy-backed BM25 lexical index with a script-aware tokenizer. tantivy's built-in tokenizers split on whitespace/punctuation — useless for scripts written without spaces between words (Thai, Lao, Khmer, Myanmar, CJK), where an entire run collapses into one unsearchable token. `ScriptAwareTokenizer` splits input into alternating no-space-script / other runs (Unicode block checks over `NO_SPACE_SCRIPT_BLOCKS`: Thai, Lao, Myanmar, Khmer, CJK Unified Ideographs + Extension A, Hiragana, Katakana, Hangul Syllables) and routes each run appropriately: no-space runs become overlapping **character bigrams** (2-char sliding windows with precise byte offsets; a single-char run emits one unigram), other runs go through the default pipeline (`SimpleTokenizer` + `RemoveLongFilter(40)` + `LowerCaser`). The bigram fallback exists because `nlpo3`'s `newmm` Thai segmenter needs a word dictionary (`words_th.txt`) that the published crate excludes — bigrams need no dictionary and are the standard CJK/Thai substring-search technique; real per-language segmenters (nlpo3, jieba, lindera) are a tracked follow-up.
**Key types**
- `ScriptAwareTokenizer` (private) + `ScriptAwareTokenStream` — the tantivy `Tokenizer`/`TokenStream` impls; registered on the index under `SCRIPT_AWARE_TOKENIZER = "script_aware"` and shared with query-time segmentation (`segment()`), so index-time and query-time tokenization always match.
- `LexIndex` — the index handle: schema fields `chunk_id` (`STRING | STORED`), `doc_path` (`STRING | STORED`), `heading_path` (`TEXT | STORED`), `body` (`TEXT`, script-aware tokenizer, `WithFreqsAndPositions`). The `IndexWriter` holds tantivy's **exclusive** directory lock, so it is created **lazily**: `open` acquires no writer lock; only the write paths materialize it on first use (`writer_mut`, 50 MB heap). Read-only opens therefore run concurrently with a writer — and past a stale lock left by a killed reindex.

**Key functions**
- `LexIndex::open(dir) -> Result<Self>` — open/create the index at `dir`, register the tokenizer; no writer lock.
- `add(&mut self, chunk: &Chunk)` — add as a new document (no dedup — callers `delete` first when re-indexing).
- `delete(&mut self, chunk_id)` — delete by exact `chunk_id` term.
- `commit(&mut self)` — make pending adds/deletes searchable; a never-written index has no writer and commits nothing.
- `search(&self, query, top_k) -> Result<Vec<(String, f32)>>` — BM25 over `body`, highest score first. Deliberately bypasses tantivy's `QueryParser` (which pre-tokenizes as English): the query is segmented with the same script-aware routine, then each **no-space-script run** (a pseudo-word like `สุขภาพ`) becomes a nested `BooleanQuery` requiring **all** of its bigrams (`with_minimum_required_clauses = n`) — exact substring-style semantics, so a hit means the document really contains the queried word, while a substring query (`ภาพ`) still matches any doc containing it; each token of an **other** run becomes one `Should` `TermQuery`. Multi-word Thai should be spaced (OR of runs); fuzzy recall is the vector side's job. Guards `top_k == 0` (tantivy's `TopDocs::with_limit` panics on 0).

**Connections** — calls: `tantivy`, `crate::chunk::Chunk`; called by: `engine::{index_doc, remove_doc, query}`.
**Tests** — English BM25, bigram matches in Thai/Chinese/Japanese/Korean/Lao, Russian via the default path, delete, whole-Thai-word-required vs substring semantics, empty/punctuation queries, `top_k == 0`, and the lazy-writer contract (concurrent read-only opens, stale `.tantivy-writer.lock` tolerated, `search` never materializes a writer).

## `src/vector.rs`
Flat mmap-backed vector store: packed `f32` rows on disk, `redb` metadata, and exact cosine top-k via `simsimd`. Vectors are assumed already L2-normalized by the embedder, so cosine reduces to a plain dot product — this store never re-normalizes.
**On-disk layout** (under the store directory): `vectors.bin` — packed little-endian `f32`, row-major, fixed stride `dims * 4` bytes (row `i` at byte offset `i * dims * 4`); `meta.redb` — tables `chunk_to_row` (chunk id → row), `row_to_chunk` (row → chunk id), `tombstones` (row → `()`; presence = deleted), `free_rows` (row → `()`; presence = reusable slot), `header` (`dims` = dimensionality, `next_row` = append cursor). Rows are read via mmap but **copied** out through `f32::from_le_bytes` into an owned `Vec<f32>` rather than cast — a `&[u8] as &[f32]` cast is unsound per Rust's alignment/aliasing rules, and one row is a few KB at most. The mmap safety comment states the store's precondition: a single writer per collection (no other process mutating `vectors.bin` concurrently).
**Key types**
- `VectorStore` — `{ dir, dims, db }`.

**Key functions**
- `open(dir, dims) -> Result<Self>` — create/open; discards a stale `vectors.bin.tmp` left by a crashed `compact`; seeds tables + header on first open; **bails if the stored `dims` differs from the requested one** (a model switch must go through `Engine::rebuild`, which recreates the store).
- `add(&mut self, chunk_id, vec)` — rejects wrong-width vectors; an existing `chunk_id` is a **replace** (old row tombstoned + freed, never a duplicate); reuses a free row if available, else appends (`next_row`). The row bytes are written **before** the metadata commit — on a reused row, committing first and crashing before the write would leave the mapping pointing at the previous occupant's vector (wrong results); write-first leaves a failure as an unreferenced row instead.
- `remove(&mut self, chunk_id)` — tombstone + push onto the free-list; no-op if absent.
- `search(&self, query_vec, top_k) -> Vec<(String, f32)>` — exact dot-product scan over all non-tombstoned rows (`simsimd` `f32::dot`), sorted descending (`total_cmp`), truncated to `top_k`. Infallible by design (empty result on a corrupt store) but never silent: transaction/table failures and skipped unreadable rows are warned to stderr with counts. `top_k == 0` or a query-dims mismatch returns empty.
- `compact(&mut self)` — rewrite `vectors.bin` dropping tombstoned rows, renumbering contiguously from 0, clearing the free-list. Crash-safe write ordering: live rows are copied into `vectors.bin.tmp`, the redb metadata is committed against the **new** numbering, and **only then** is the tmp renamed over `vectors.bin` — a crash before the commit leaves the old metadata describing the old, intact file (the tmp is unreferenced scratch discarded by the next `open`). The prior order (rename first) could leave old row numbers indexing the new file: silent, irrecoverable corruption.
- `len()` / `is_empty()` — live (non-tombstoned) row count.

**Connections** — calls: `memmap2`, `redb`, `simsimd`; called by: `engine::{index_doc, remove_doc, query, vector_search, rebuild_inner}`.
**Tests** — add/search/remove roundtrip, reopen persistence, dims-mismatch rejection (open and add), replace-not-duplicate, free-row reuse (the file doesn't grow a third row: `2 * 3 * 4` bytes asserted), remove-missing no-op, search edge cases, compact (drops tombstones, no-op, empty store, tmp consumed), and leftover-tmp discard on open.

## `src/hybrid.rs`
Reciprocal Rank Fusion of the lexical and vector result lists. RRF uses only each result's *rank* (0-based position) within its own list, never its raw score — sidestepping the fact that BM25 scores and cosine similarities live on incomparable scales.
**Key functions**
- `rrf_fuse(lex, vec, k, top_k) -> Vec<(String, f64)>` — each list contributes `1.0 / (k + rank)` per chunk_id, summed across lists; sorted by fused score descending with a chunk_id tiebreak for deterministic output (`total_cmp` — no NaN panic, matching `src/vector.rs`'s sort), truncated to `top_k`. `k` is a parameter here; the production constant `RRF_K = 60.0` lives in `src/engine.rs`. `top_k` here is widened to `max(caller's top_k, search.reranker.candidates)` by the caller (`engine::query`) so the Tier-2 rerank stage sees a full candidate block, not just the caller's requested count.

**Connections** — calls: nothing (pure); called by: `engine::query`.
**Tests** — both-lists ranking (a chunk present in both lists ranks first), `top_k` truncation, empty inputs.

## `src/rerank.rs`
Cross-encoder reranking: the Tier-2 precision stage over already-retrieved candidates ([ADR 0025](../decisions/0025-tier2-cross-encoder-reranker.md)). Unlike the embedder/vector store, this module scores a **query/passage pair** directly (not two independently-embedded vectors), giving a calibrated 0–1 relevance score that can actually gate irrelevant results rather than merely hint at them ([ADR 0024](../decisions/0024-vector-confidence-recall-first.md) found bi-encoder cosine could not do this cleanly).
**Key types**
- `trait Rerank` — the reranking seam: `rerank(query, passages) -> Result<Vec<f32>>`, same order as `passages`. `Send + Sync`, mirroring `embed::Embed` — the engine holds its boxed reranker behind an `Arc<Mutex<_>>`-equivalent shared-across-threads design.
- `Reranker` (behind `#[cfg(feature = "semantic")]`) — wraps a `fastembed::TextRerank` behind a `Mutex` (`rerank` takes `&mut self` on the model); constructed once via `rerank::new`.
- `FakeReranker` — deterministic in-memory stand-in (Jaccard token overlap between query and passage, mapped through `sigmoid`) used by engine tests so query/rerank logic runs with no model download.
- `RerankerInfo` — one registry entry: `name`, `approx_size`/`approx_bytes`, `max_length` (context, in tokens), `note`, `hf_repo`, `model_file`, `sha256`. Mirrors `embed::ModelInfo`'s shape.

**The registry** (`reranker_registry()` — single source of truth for reranker names; currently one entry):

| name | approx size | max length | note |
|---|---|---|---|
| `onebrain-reranker-v1` | ~570 MB | 512 | `bge-reranker-v2-m3` base, int8, multilingual incl. Thai |

The registry name is **versioned** (`onebrain-reranker-v1`, `-v2`, …), independent of the upstream base model's own naming — each version is a distinct entry, **sha256-pinned** to one specific model file, so a base-model upgrade is always a new registry entry, never a silent swap under an existing name. See [ADR 0025](../decisions/0025-tier2-cross-encoder-reranker.md) for the model-choice rationale (why int8 over fp32, why not `jina-reranker-v2` or `BGE-reranker-base`).

**Key functions**
- `new(model_name, cache_dir) -> Result<Reranker>` (behind `semantic`) — resolves the registry entry, downloads via `hf-hub` (same `models--org--repo` cache convention as `embed.rs`), verifies the model file's sha256 once (`verify_sha256_once`, short-circuited on later loads via a `.sha256-verified` marker file next to the model), loads the tokenizer + `TextRerank`.
- `is_supported_reranker(name) -> bool` — derived from `reranker_registry`, never out of sync.
- `reranker_download_status(info, cache_dir) -> embed::ModelDownloadStatus` — pure `std::fs` presence + size check, reusing `embed`'s status type; never downloads, never opens a model.
- `sigmoid(logit) -> f32` — maps a raw cross-encoder logit to a calibrated (0,1) probability; also used by `FakeReranker` to keep its stand-in scores in the same shape as the real model's output.
- (private: `marker_path`, `verify_sha256_once`, `tokenize`, `jaccard` — the sha256-verification and `FakeReranker` helpers.)

**Connections** — calls: `fastembed::{TextRerank, ...}` (behind `semantic`), `hf-hub`, `sha2`; called by: `engine::{reranker, build_reranker}` (lazy construction), `search_reindex`'s reranker-fetch step, and directly by `onebrain-cli`'s `search model`/`search status`/`doctor` surfaces (registry, download status).
**Tests** — registry invariants (exactly one seeded entry, metadata matches, cache-dir naming, name lookup), `sigmoid` bounds/monotonicity/midpoint, `FakeReranker` determinism/ordering/bounds/empty-input, sha256 verify pass/mismatch/marker-short-circuit, plus a network-gated real-rerank test (`ONEBRAIN_TEST_RERANK`, skipped by default — the model repo may not yet exist on HF when this test file was written).

## `src/engine.rs`
Ties `chunk` + `embed` + `vector` + `lex` + `hybrid` into one synchronous engine. Owns the tuning constants: `CHUNK_MAX_TOKENS = 512`, `CHUNK_OVERLAP_TOKENS = 64`, `LEX_TOP_K = 50`, `VEC_TOP_K = 50`, `RRF_K = 60.0`, `SNIPPET_MAX_CHARS = 200`. Its `engine.redb` holds five tables — `chunk_meta` (chunk id → serialized `ChunkMeta`), `doc_chunks` (doc path → JSON chunk-id list), `doc_hashes` (doc path → sha256 hex), `lex_hashes` (doc path → sha256 hex for lex-only indexing state), `engine_header` (`active_model`, `last_indexed_at`) — all string-keyed, values serialized with `serde_json`. Neither `lex` nor `vector` stores the chunk's text or heading path, so `engine.redb` is the only source `get` and `Hit` snippets can draw from.
**Key types**
- `Engine` — `{ lex: LexIndex, vec: VectorStore, exclude_patterns, model_name, cache_dir, embedder: EmbedSource, meta: Database }`.
- `EmbedSource` (private) — how the engine obtains its embedder: `Lazy(OnceCell<Box<dyn Embed>>)` in production (deferred `fastembed` construction) or `Injected(Box<dyn Embed>)` via the `#[cfg(test)]` seams. `Engine::embedder()` is the **only** place `embed::new` is called — the first `index_doc`/`query`/`vector_search`/`rebuild` is when a model download actually happens, never `open`.
- `Hit` — a fused, resolved result: `{ chunk_id, doc_path, heading_path, score, snippet, rerank_score }` (snippet char-boundary-truncated to 200 chars + `…` — safe for multibyte Thai). `rerank_score: Option<f32>` is the Tier-2 cross-encoder's calibrated 0–1 score; `None` means the hit is unreranked (stage skipped, failed, or the hit sits in the fused tail beyond `candidates`).
- `ChunkMeta` (private) — the stored per-chunk record `{ doc_path, heading_path, chunk_index, text }`.
- `ReindexProgress` — live progress events (the engine is UI-free): `Walked { total }` (exactly once, right after the file walk), `LoadingModel` (at most once, right before the run's **first** embed call — the model-download/load stall point; a run with nothing to (re)embed never emits it), `Indexing { done, total, doc_path }` (after each processed doc), `LoadingReranker` (at most once, at the END of a full reindex, right before the engine constructs the reranker for the first time — the reranker-download analogue of `LoadingModel`; only fires when reranking is enabled and the configured model isn't yet downloaded — see `should_fetch_reranker`).
- `ReindexStats` — `{ added, updated, removed, unchanged, failed }` (`failed`: unreadable/failing files are counted and skipped, never abort the batch).
- `IndexStatus` — `status` snapshot: `{ doc_count, last_indexed_at: Option<u64>, pending_new, pending_changed, pending_removed }` + `pending_total()`; the `pending_*` counts are exactly the diff a reindex would act on (add/update/remove), minus the indexing.
- `HashDiff` (private) — `Added | Updated | Unchanged`, from the pure `diff_hash(stored, current)` classifier.

**Key functions**
- `Engine::open(cache_dir, embed_model) -> Result<Self>` — open/create everything under `cache_dir` (`tantivy/`, `vectors/` at `embed::model_dims(embed_model)`, `engine.redb`), record `embed_model` as active if none recorded; embedder stays lazy — **never downloads**.
- `Engine::open_with_embedder(..)` / `rebuild_with_embedder(..)` — `#[cfg(test)]` seams injecting a pre-built embedder; the deterministic fake (`FakeEmbedder`: hashes whitespace tokens into `dims` buckets, then L2-normalizes — identical text ⇒ identical vector, cosine 1.0 for exact-text queries) lives in this file's test module.
- `set_exclude_patterns(&mut self, patterns)` — install the vault's `search.exclude` patterns, applied by every vault walk on top of the built-in skips.
- `index_doc(&mut self, doc_path, content) -> Result<usize>` — chunk (512/64), batch-embed all chunk texts in **one** `embed_passages` call, add each chunk to lex + vector, record `ChunkMeta` + the doc's chunk-id list, commit lex; returns the chunk count.
- `remove_doc(&mut self, doc_path)` — look up the doc's chunk ids, delete each from lex + vector + `chunk_meta`, drop the `doc_chunks` entry.
- `query(&self, text, top_k) -> Result<Vec<Hit>>` — hybrid search: `embed_query` → vector top-50 trimmed to the top cluster (`keep_top_cluster`, within `VEC_CLUSTER_WINDOW` of the best score — recall-first, relative) + lex top-50 → `rrf_fuse(.., RRF_K, fuse_k)` (`fuse_k = max(top_k, rerank_settings.candidates)`, wide enough for the rerank stage) → `apply_rerank` (Tier-2 cross-encoder stage, [ADR 0025](../decisions/0025-tier2-cross-encoder-reranker.md)) → `resolve_hits`.
- `vector_search(&self, text, top_k)` — vector-only semantic search (no lex/RRF fusion), fetched at `max(top_k, candidates)` and passed through the same `apply_rerank` stage; the CLI's `search vsearch` verb.
- `apply_rerank(&self, query, fused, top_k) -> Result<Vec<Hit>>` (private) — the Tier-2 rerank stage shared by `query` and `vector_search`: split the fused/retrieved list into a head (`candidates`-sized) and tail; no reranker resolved → return the fused order unreranked; else cross-encode the head's chunk texts (`chunk_texts`) against `query`, sort by calibrated score descending (chunk-id tiebreak), gate at `rerank_settings.min_score` with a never-empty floor (`RERANK_NO_MATCH_KEEP = 3` survivors on total rejection), append the untouched tail with `rerank_score: None`, truncate to `top_k`, resolve. A rerank call error degrades the same way as no-reranker (skip-not-fail) after a once-per-engine stderr log.
- `chunk_texts(&self, ids) -> Result<Vec<String>>` (private) — full stored chunk text for each candidate id, index-aligned (missing/corrupt meta yields an empty string rather than failing the batch — corruption is logged once per engine, then quiet).
- `should_fetch_reranker(enabled, downloaded) -> bool` (private, pure) — the reindex-time decision: fetch the reranker only when reranking is enabled AND not already downloaded; an unknown model name is treated as "downloaded" by the caller so this function stays registry-agnostic.
- `reranker(&self) -> Option<&dyn Rerank>`, `build_reranker`, `set_rerank_settings`, `rerank_enabled`, `rerank_active`, `set_reranker_for_tests` (`#[cfg(test)]`) — the `RerankSource`/`RerankSettings` plumbing mirroring `EmbedSource`'s lazy-construction pattern, with one added honesty rule: the lazy path resolves to `None` (skip, never fail) when the model isn't downloaded, the build lacks `semantic`, or construction fails.
- `get(&self, doc_path) -> Result<String>` — full stored text: the doc's chunks concatenated in `chunk_index` order; error if absent.
- `status(&self, vault_root) -> Result<IndexStatus>` — read-only drift report: snapshot stored hashes, walk + re-hash the vault's `*.md` files, classify each via `diff_hash`, count indexed docs whose file is gone. Never constructs the embedder.
- `reindex_paths(&mut self, vault_root, doc_paths)` / `reindex_paths_with_progress(..)` — reindex specific vault-relative paths: on-disk files go through the added/updated/unchanged hash logic; missing-but-indexed paths are removed; neither-on-disk-nor-indexed is ignored.
- `reindex_all(&mut self, vault_root)` / `reindex_all_with_progress(..)` — walk the whole vault (`walk_markdown_files`: `*.md` **only**; hidden dirs and `node_modules` always skipped; `exclude` patterns applied — entries containing `/` are path prefixes, bare names match any path component), reindex each (per-file failures counted in `stats.failed` + stderr, never aborting), then sweep stale docs (any doc present in `doc_hashes` or `lex_hashes` but not seen on disk — `HashSet` membership, O(N) — `remove_doc` + both hash entries dropped) and record `last_indexed_at`.
- `rebuild(&mut self, new_model)` / `rebuild_with_progress(..)` — model switch: collect the re-embed worklist from `chunk_meta`, delete + recreate **only** the vector store at the new model's dims, reset to a fresh lazy embedder, re-embed in batches of 64 (progress `(0, total)` fires before the first embed — the download/load stall), record the new active model. The lex index and `chunk_meta`/`doc_chunks` are model-independent and untouched; an empty index never constructs the embedder.
- `active_model_matches(&self, cfg_model) -> Result<bool>` — compares the `engine_header` active model against config (the CLI's rebuild-needed probe).
- `short_path_hash(path) -> String` — first 6 hex chars of sha256(path); public so the CLI derives the default collection name (`<dir>-<hash>`) without duplicating the hashing.
- (private helpers: `hash_bytes` (sha256 hex), `diff_hash`, `vault_relative_path` (forward slashes on every platform — the stable `doc_path` key), `is_skipped_dir`, `is_excluded`, `walk_markdown_files`, `keep_top_cluster`, `truncate_snippet`, `now_epoch_secs`.)

**Connections** — calls: every sibling module + `redb`, `sha2`, `serde_json`; called by: onebrain-cli's `search` verb group via `search_common.rs` (`Engine::open(collection_cache_dir(..), config.search.embed_model)`) and `doctor`'s `native_search_check`.
**Tests** — ~60 tests, almost all against `FakeEmbedder`/`FakeReranker` (no network): index→query→get→remove roundtrips, add/update/unchanged/remove drift detection for both reindex paths, `status` doc-count/last-indexed/drift, `ReindexProgress` event contracts (increasing `done`, `LoadingModel` before the first embed, `LoadingReranker` truth-table via `should_fetch_reranker`), rebuild (re-embeds all, dims change, empty index embeds nothing, batched progress), failed-file counting (root-guarded chmod test), `keep_top_cluster` recall-first cutoff (within-window / never-empties / empty), exclude-pattern and hidden-dir/`node_modules` walk behavior, `apply_rerank`'s gate/never-empty-floor/skip-not-fail/tail-passthrough behavior against `FakeReranker`, and the pure helpers (`vault_relative_path`, `short_path_hash`, `diff_hash`, snippet truncation on a char boundary).

## Entry points
The public surface other crates (chiefly `onebrain-cli`) reach for first:
- **Engine** — `engine::Engine` (`open`, `set_exclude_patterns`, `index_doc`, `remove_doc`, `query`, `vector_search`, `get`, `status`, `reindex_all[_with_progress]`, `reindex_paths[_with_progress]`, `rebuild[_with_progress]`, `active_model_matches`, `set_rerank_settings`, `rerank_enabled`, `rerank_active`), `engine::{Hit, IndexStatus, ReindexStats, ReindexProgress, RerankSettings, DEFAULT_RERANK_MIN_SCORE}`, `engine::short_path_hash`
- **Model registry** — `embed::{model_registry, ModelInfo, is_supported_model, model_dims, model_download_status, ModelDownloadStatus, dir_size_bytes}`, `embed::{new, new_quiet, Embedder, Embed}`
- **Reranker registry** — `rerank::{reranker_registry, RerankerInfo, is_supported_reranker, reranker_download_status}`, `rerank::{new, Reranker, Rerank}`
- **Building blocks** (rarely used directly) — `chunk::{chunk_markdown, Chunk}`, `lex::LexIndex`, `vector::VectorStore`, `hybrid::rrf_fuse`

## MCP server

The engine is also reachable over MCP (Model Context Protocol) via the `onebrain mcp` stdio server, hosted in `onebrain-cli` (`commands/mcp.rs`) — tool handlers drive this crate's synchronous `Engine` through `spawn_blocking`, so the engine itself stays async-free. Tool schemas, parameter tables, result shapes, and client-registration snippets are documented separately as an API reference, not a code map: see **[`docs/reference/mcp.md`](mcp.md)**. Architecture rationale (why a top-level command, the qmd-compatible tool surface, the staged plugin-config cutover) is in [ADR 0019](../decisions/0019-native-mcp-server-staged-qmd-cutover.md).

## Choosing an embedding model

`search.embed_model` in `onebrain.yml` selects which of the six [registry](#srcembedrs) models embeds your vault. Switching later means a full re-embed (the vector store is opened at the new model's `dims`, so nothing carries over) — a one-time, minutes-scale cost on a personal vault, not something irreversible, but still worth picking deliberately rather than churning.

### Comparison

Every column below traces to the registry entry in `crates/onebrain-search/src/embed.rs` (dims, download size, and Thai MIRACL score are read directly from `ModelInfo`; nothing here is invented). The two speed columns are **not** measured — they're parameter-count-based estimates, labeled accordingly.

| Model | Dims | Download | Approx RAM loaded | Embed/reindex speed (est.) | Query latency (est.) | THAI (MIRACL nDCG@10) | Note |
|---|---|---|---|---|---|---|---|
| `multilingual-e5-small` | 384 | ~470 MB | ~0.5–1 GB | ×1 (baseline) | ×1 (baseline) | 75.0 | default · small + fast |
| `multilingual-e5-base` | 768 | ~1.1 GB | ~1–1.5 GB | ×2–2.5 | ×2–2.5 | 75.2 | larger · better recall |
| `multilingual-e5-large` | 1024 | ~2.1 GB | ~2–2.5 GB | ×4–5 | ×4–5 | 80.2 | high accuracy |
| `bge-m3` | 1024 | ~2.2 GB | ~2.5–3 GB | ×4–5 | ×4–5 | 82.6 | best Thai/accuracy · fp32 |
| `embeddinggemma-300m-q` | 768 | ~310 MB | ~0.5–1 GB | ×0.5–1 | ×0.5–1 | unverified | small · int8 · Thai unverified |
| `embeddinggemma-300m-q4` | 768 | ~200 MB | ~0.5 GB | ×0.5–1 | ×0.5–1 | unverified | smallest · 4-bit · Thai unverified |

RAM-while-loaded figures are rough (model weights + ONNX Runtime session overhead); they scale with download size, not dims, since dims only affects the (much smaller) per-vector storage cost.

### Where the cost actually goes

| Cost | Bound by | Notes |
|---|---|---|
| Reindex / embed | **model size** (dominant cost) | Paid on every reindex, and in full again on any model switch (`rebuild` re-embeds everything). |
| Query latency | **model size** | One forward pass per query — cheap in absolute terms even for large models, since it's a single short string, not a batch. |
| Vector scan | dims | Negligible at personal-vault scale (thousands of chunks, exact scan) — dims only matters at a scale this engine isn't targeting. |
| RAM / disk | model size + `dims × 4 bytes` per chunk | Model size dominates; per-chunk vector storage is a rounding error by comparison. |

### Decision guide

- **Default**: `multilingual-e5-small` — fast, light, fine for general multilingual use including baseline Thai.
- **Thai-accuracy-first, ≥16 GB RAM**: `bge-m3` (THAI 82.6) or `multilingual-e5-large` (80.2). Note `multilingual-e5-base`'s THAI 75.2 vs. small's 75.0 is **not** a meaningful Thai upgrade — pick `base` for better English/general multilingual recall on a budget, not for Thai specifically.
- **Low-RAM (8 GB)**: `multilingual-e5-small` or an `embeddinggemma` variant (Thai unverified for gemma — don't pick it for Thai accuracy without testing your own corpus).
- **Big vaults (10k+ docs)**: embed time scales linearly with doc count — prefer `small`/`base` unless you've already budgeted for a large-model reindex.
- **Switching later** = a full re-embed (dims change invalidates the existing vector store). Budget minutes, not hours, on a personal vault — but choose deliberately rather than switching repeatedly.

### Hardware & acceleration

Embedding inference runs on **CPU**, via ONNX Runtime's CPU execution provider — a deliberate packaging choice, not an architecture ceiling and not related to multilingual support (language capability lives in the model, not the compute device). Why CPU-only: the single-binary/no-runtime-deps principle (a GPU build would bind CUDA/cuDNN dynamic libraries on every user's machine, whether or not they have a compatible GPU) plus release-matrix cost — see [ADR 0018](../decisions/0018-release-build-strategy-lessons.md) on how much a single added native dependency multiplies the 9-target release matrix. Full rationale: [ADR 0020](../decisions/0020-cpu-only-embedding-runtime.md).

In practice: Apple Silicon's CPU inference is well-optimized (NEON), and query-time cost is one forward pass per query, so the latency-sensitive path is cheap regardless of accelerator. Reindex/embed of a large vault on a large model is where CPU-only inference costs the most wall-clock time.

**Future acceleration ladder** (roadmap — not a commitment, not scheduled):

1. **CoreML execution provider** on macOS via an `ort` feature flag — no extra user-facing dependency.
2. **CUDA / DirectML builds** as separate release artifacts, produced only on demand.
3. **External embedding endpoints** (e.g. MLX, Ollama) via the engine's existing `Embed` trait seam (`crates/onebrain-search/src/embed.rs`) — an additive integration point, not a rearchitecture.

## CLI commands

The CLI surface in `onebrain-cli` (`commands/search_reindex.rs`, `commands/search_query.rs`, `commands/search_status.rs`, `commands/search_get.rs`, `commands/search_model.rs`) exposes the engine via `onebrain search <verb>`:

- `onebrain search reindex [PATHS]` — reindex the vault or specific paths. Flags:
  - `--lex-only` — incremental keyword-index pass; never loads or downloads the embedding model. Changed docs' vectors stay pending until the next embed pass (`--pending-only`). Safe to call from a hook — it never prompts and never fails the calling turn (errors degrade to a skip envelope, exit 0).
  - `--pending-only` — embed only docs whose vectors are pending (from a previous `--lex-only` pass, or external edits). Loads the model only when there is pending work. Safe to call from a hook — it never prompts and never fails the calling turn (errors degrade to a skip envelope, exit 0).
  - Both flags silently skip (exit 0, `skipped`/`reason` JSON envelope) when: no collection configured · no index exists · model not downloaded · a reindex is already running · **the engine is locked by another process** (`reason: "engine-busy"` — a locked index must never break the hook chain). See [ADR 0022](../decisions/0022-honest-search-lock-errors.md).
  - A full reindex also fetches the Tier-2 reranker model (`LoadingReranker` progress phase) when `search.reranker.enabled` and it isn't already downloaded — the query path never downloads it.
- `onebrain search query <TEXT>` — hybrid keyword + semantic search (Reciprocal Rank Fusion), then the Tier-2 rerank stage over the fused candidates ([ADR 0025](../decisions/0025-tier2-cross-encoder-reranker.md)). Exits **77** with an `E_ENGINE_BUSY` envelope if the engine is locked by another process (redb is single-process — typically the `onebrain mcp` server holds it); same for `vsearch` / `get` / full `reindex`.
- `onebrain search search <TEXT>` — keyword-only search (tantivy BM25). Reads `heading_path` from the STORED tantivy field (no redb open, so it's never blocked by the lock); `snippet` is deferred and stays empty on this verb — the `body` field is indexed but NOT STORED, so a snippet needs a schema change + reindex migration. Empty `snippet` here means "not supported yet", NOT "no matching text". Never reranked (no candidates to cross-encode without an embedder/vector leg).
- `onebrain search vsearch <TEXT>` — semantic-only search (vector similarity), passed through the Tier-2 rerank stage same as `query`; unavailable on lex-only binaries.
- `onebrain search get <DOC_PATH>` — fetch a document's stored text + metadata.
- `onebrain search status` — report index state (doc count, last reindex timestamp, pending drift) plus a Reranker section (`reranker_model`, `reranker_ready`, `reranker_downloaded`, `reranker_disk_bytes`). Reports contention/breakage **honestly and never shows "✅ up to date" over an unknown index** (all exit 0 — a report, not a failure): a live lock → `busy: true` + `W_ENGINE_BUSY` + null counts; a broken/unreadable index (status-read failure or a non-lock open failure) → `status_error` + `W_STATUS_UNREADABLE` + null counts. See [ADR 0022](../decisions/0022-honest-search-lock-errors.md).
- `onebrain search model [list|set|remove]` — list available embedding models (plus a Rerankers table, keyed off `search.reranker.model`) or switch/remove a model.

All search commands support `--json` / `--yaml` structured output modes.
