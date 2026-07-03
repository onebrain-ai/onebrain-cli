# 0020 — CPU-only embedding runtime, by packaging choice

- **Status:** accepted
- **Date:** 2026-07-03

## Context

`onebrain-search`'s embedding inference (`crates/onebrain-search/src/embed.rs`, `Embedder` wrapping `fastembed`) runs entirely on ONNX Runtime's CPU execution provider. Read in isolation, "CPU-only" can be misread two ways worth ruling out explicitly: as a hard architecture ceiling, or as evidence the models themselves are somehow limited in language coverage. Neither is true — it's a deliberate packaging choice, made for the same reasons [ADR 0017](0017-platform-tiered-semantic-search.md) and [ADR 0018](0018-release-build-strategy-lessons.md) already committed to: OneBrain ships as a single static binary with no runtime dependencies the user has to install, and every native dependency added multiplies the release-matrix surface (9 target triples as of ADR 0018).

A GPU execution provider (CUDA, DirectML, CoreML) would mean either bundling provider-specific dynamic libraries (CUDA/cuDNN `.so`/`.dll` files users would need on their machine regardless of whether they have a compatible GPU) or shipping separate build artifacts per accelerator per platform — both directly against the single-binary, zero-runtime-deps principle this CLI has held since the Rust rewrite ([ADR 0001](0001-rust-rewrite.md)).

## Decision

- Embedding inference stays on ONNX Runtime's **CPU execution provider** for all shipped binaries, for all platforms, indefinitely — not as a stopgap, but as the default posture consistent with the single-binary principle.
- This is explicitly **not** a multilingual or model-capability limitation: language coverage is a property of the chosen model (see the registry in `crates/onebrain-search/src/embed.rs` — `multilingual-e5-*`, `bge-m3`, `embeddinggemma-*`, all multilingual-trained), not of the compute device running it. A model doesn't "know less" about Thai because it runs on CPU instead of GPU.
- CPU inference is well-optimized on the platforms OneBrain users are most likely to run on: Apple Silicon's NEON vector units handle ONNX Runtime's CPU kernels efficiently, and query-time cost is a single forward pass per query (embedding one short query string), not a batch workload — the latency-sensitive path is cheap regardless of accelerator.
- A **future acceleration ladder** is recorded here as roadmap, explicitly **not a commitment or a scheduled deliverable**:
  1. **CoreML execution provider on macOS**, gated behind an `ort` feature flag — no extra user-facing dependency (CoreML ships with macOS), so this is the lowest-cost rung.
  2. **CUDA / DirectML builds** as separate release artifacts, produced only on demand (e.g. if a benchmark or user demand justifies the added release-matrix surface) — never folded into the default single binary.
  3. **External embedding endpoints** (e.g. MLX, Ollama) reached through the engine's existing `Embed` trait seam (`crates/onebrain-search/src/embed.rs`) — the trait already separates "how to turn text into a vector" from the rest of the engine (chunking, lexical index, vector store, fusion), so an HTTP-backed `Embed` implementor is an additive seam, not a rearchitecture.

## Consequences

- No CUDA/cuDNN dylibs ship or are ever assumed present on a user's machine; the 9-target release matrix ([ADR 0018](0018-release-build-strategy-lessons.md)) does not multiply per-accelerator.
- Embed/reindex throughput is bounded by CPU inference speed — for the larger registry models (`bge-m3`, `multilingual-e5-large`) this is the dominant cost of a reindex or a model switch (see the "Where the cost actually goes" table in [`docs/reference/onebrain-search.md`](../reference/onebrain-search.md#choosing-an-embedding-model)). Users who want faster large-model reindexing today have no in-box lever beyond picking a smaller model; that's a real tradeoff, made deliberately rather than being an oversight.
- The acceleration ladder above gives a concrete, ordered path if/when GPU throughput becomes a real bottleneck for real users, without pre-committing engineering time before that evidence exists.
