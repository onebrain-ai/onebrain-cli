# 0012 — Native Rust search engine (replace external qmd)

- **Status:** accepted
- **Date:** 2026-07-01

## Context

Vault search (keyword + semantic) was delegated to **`@tobilu/qmd`**, a third-party Node.js CLI installed separately (npm). OneBrain merely shelled out to it and re-exposed its `qmd mcp` server to the agent. Three problems compounded:

- **Not ours to control.** Version, roadmap, and bugs of the search engine lived in someone else's repo.
- **Node-ABI install pain.** qmd's native addons (`better-sqlite3`, `node-llama-cpp`) break across Node major versions (hit on Node v26). Every machine needed a working Node + a separate install of qmd.
- **Two things to install.** Users installed the OneBrain CLI *and* qmd; the CLI is otherwise a single self-contained binary.

The Rust ecosystem now covers every piece qmd provided, with permissive licenses and no Node/Python: `tantivy` (BM25), `nlpo3` (Thai word segmentation), `fastembed` (ONNX embeddings via a statically-linked runtime — no runtime-ABI class), flat brute-force vectors (`simsimd` + `memmap2`), and `rmcp` (an MCP server).

## Decision

Build a native **`onebrain-search`** engine crate and remove qmd. Key sub-decisions:

- **Rebuild, not port.** qmd's value is its ranking pipeline (RRF + LLM rerank + query expansion), a few hundred lines re-implemented either way; a line-by-line JS port buys nothing and drags in the node-llama-cpp binding. Build fresh on tantivy + fastembed.
- **One engine, two frontends.** A sync engine core (chunk → tantivy lex + fastembed vec → RRF fuse) is exposed by **(a)** OneBrain's own native **MCP server** (`rmcp`, replacing qmd's) — the agent's typed-tool path — and **(b)** `onebrain search …` **CLI verbs**. Only qmd's MCP is dropped, not MCP access. `tokio` lives only at the MCP edge; the engine stays sync.
- **Default embedding model = `multilingual-e5-small`** (~470 MB, Thai MIRACL-th 75.0). fastembed 5.17 has **no quantized bge-m3** (`BGEM3Q` does not exist — verified against the crate; an early research claim of "~542 MB quantized" was wrong), so bge-m3 is only fp32 `BGEM3` (~2.2 GB) — too large for the default. bge-m3 (Thai 82.6) is the **accuracy upgrade**, chosen via an interactive `onebrain search model` picker that shows each model's size + Thai score so the choice can't be a blind typo. The model is **swappable** and swapping re-embeds only the vector store (the lex index is model-independent); a `{model, dims}` header guards against mixed-model vectors.
- **Thai handled honestly.** tantivy's default tokenizer can't segment space-less Thai, so a custom tokenizer emits **character bigrams** over each Thai run (a dictionary-free fallback); proper `nlpo3` word segmentation (dictionary newmm) is deferred. Semantic Thai is carried in the meantime by the multilingual embeddings on the vector side.
- **Chunking** is heading-aware + size-split, carrying each chunk's heading path.
- **Phasing (v3.4.x):** v3.4.0 engine + CLI verbs (alongside qmd, for validation) → v3.4.1 native MCP + swap plugin MCP config → **v3.4.2 cutover** (auto reindex/embed via hook, `/qmd`→`/search` skill, re-embed migration, **remove qmd** — 0 node/python dep) → v3.4.3 polish (rerank, query expansion, bge-m3 sparse). The `/qmd` **skill is removed** (merged into `/search`) — plugin-repo (`onebrain-ai/onebrain`) work, each milestone with its own plan.

The engine architecture and module layout are documented in the `onebrain-search` crate's own docs (`crates/onebrain-search/src/lib.rs`, `cargo doc -p onebrain-search`).

## Consequences

- **One binary, nothing extra to install.** No Node, no separate search tool, no native-addon ABI breakage. Models auto-download to a cache on first `search reindex`, with progress.
- **We own the whole stack** — ranking, indexing, and the search MCP are ours to tune and ship on our own cadence.
- **Smaller/faster default** (e5-small) with a one-command upgrade path to best-in-class Thai (bge-m3) for users who want it.
- **Costs:** a one-time full re-embed on cutover (minutes); a custom Thai tokenizer to carry; a first-download UX to get right; and a two-repo migration (CLI + plugin) to sequence. tantivy is pre-1.0, so its version is pinned.
