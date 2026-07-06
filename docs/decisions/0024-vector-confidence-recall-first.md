# 0024 — Recall-first vector cutoff + honest confidence (retire the absolute floor)

Status: accepted (v3.4.6) — **superseded in part by [ADR 0025](0025-tier2-cross-encoder-reranker.md)** (v3.4.7): the Tier-2 cross-encoder reranker is now the real precision gate for every reranked hit. `vec_confidence_hint`'s bi-encoder cosine heuristic (below) is **not removed** — it remains the fallback confidence signal for the narrow case where a hit genuinely has no `rerank_score` to band on (reranker disabled, not downloaded, or a lex-only build). `keep_top_cluster` (the recall-first vector cutoff itself) is unaffected — reranking runs on top of it, not instead of it.

## Context

Native vector search dropped hits below a per-model absolute cosine floor
(`ModelInfo::vec_floor`, 0.85 for the e5 family; `drop_below_floor`). The intent
was to suppress noise: a query about something the vault doesn't contain would
otherwise surface its nearest neighbours as authoritative-looking results.

Pre-tag testing on the real ob-1 vault (759 docs) showed the floor was
**mis-calibrated and failing silently** (recorded in the project vault's
`2026-07-06-vsearch-floor-calibration-finding` note): `vsearch` returned
**0 hits** for genuine queries, and
`query` (hybrid) silently degraded to lex-only, because e5 genuine matches
cluster at ~0.83–0.87 — *straddling* the 0.85 floor and overlapping the e5
unrelated-baseline (~0.84).

Measured distributions (floor off, 10 match vs 10 no-match queries):

| signal | match | no-match |
|---|---|---|
| top1 | mean 0.857, range **0.822–0.874** | mean 0.827, range **0.799–0.850** |

The ranges **overlap** — there is **no absolute threshold** that keeps all
genuine matches and drops all noise. A bi-encoder cosine simply cannot cleanly
gate relevance for e5 on this content. So the floor was destroying recall
(and doing it invisibly: `semantic_available: true`, no warning).

## Decision

Retire the per-model absolute floor. Replace it with a **recall-first, honest**
pair of mechanisms (model-agnostic — they work on each query's own result shape,
not a hand-tuned per-model constant):

1. **`keep_top_cluster(hits, window)`** (`engine.rs`) — keep the coherent top
   cluster (hits within `VEC_CLUSTER_WINDOW = 0.02` cosine of the query's best
   score). **Never empties a non-empty set.** Used by both `query` (so vec
   contributes to RRF again — lex + fusion provide precision) and `vector_search`.
2. **`vec_confidence_hint(top_score)`** (`search_query.rs`) — an advisory,
   **never-silent** label from the best raw cosine: ≥0.86 confident (no hint);
   0.80–0.86 low-confidence; <0.80 "no strong match". The renderer now surfaces
   this hint on **non-empty** results too — a weak `vsearch` reads as honest
   rather than authoritative. **Scope: `vsearch` (vector-only) only.** `query`
   (hybrid) returns a fused RRF score, not a raw cosine, so there is no
   comparable per-result confidence to label in Tier 1; hybrid leans on its lex
   half for precision. A narrow consequence: a query with *no* lex match and
   only weak vec neighbours now returns those neighbours (recall-first) where the
   old floor would have returned empty — best-effort, but unlabelled on the
   hybrid path. Per-result confidence for **all** verbs/surfaces (CLI, MCP,
   daemon, webui) arrives with the Tier-2 cross-encoder reranker, whose
   calibrated 0–1 score is a real gate (below).
3. **Deleted `ModelInfo::vec_floor`** (field + registry values + `Engine::vec_floor`).

## Consequences

- **Recall restored**: genuine matches (e.g. 0.839) are returned, with an honest
  confidence label. `query` no longer silently collapses to keyword-only.
- **Trade-off accepted**: because match/no-match overlap, we cannot perfectly
  gate noise at this stage — a no-match query returns its nearest cluster,
  *labelled* low/no-confidence. This is the honest choice (recall + disclosure)
  over silent precision that also drops real answers. Bi-encoder confidence is
  fundamentally limited here.
- The confidence bands (0.86/0.80) are e5-calibrated globals; a mislabel for
  another model only affects the advisory text, never what's returned.

## The real fix landed next (Tier 2, v3.4.7)

This ADR was the **Tier-1 stopgap**. The proper fix — as qmd (`@tobilu/qmd`) did —
is a **cross-encoder reranker**: retrieve recall-first, then rerank the top-K.
That reranker (`onebrain-rerank-v1`, a `bge-reranker-v2-m3`-based int8
cross-encoder) landed in v3.4.7 — see [ADR 0025](0025-tier2-cross-encoder-reranker.md).
Its calibrated 0–1 score **replaces this confidence heuristic** as a reliable
gate for every hit it actually reranks; the heuristic below now serves only
the unreranked-fallback case (see the status note at the top of this ADR).
