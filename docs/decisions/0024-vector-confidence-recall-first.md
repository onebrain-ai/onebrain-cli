# 0024 — Recall-first vector cutoff + honest confidence (retire the absolute floor)

Status: accepted (v3.4.6)

## Context

Native vector search dropped hits below a per-model absolute cosine floor
(`ModelInfo::vec_floor`, 0.85 for the e5 family; `drop_below_floor`). The intent
was to suppress noise: a query about something the vault doesn't contain would
otherwise surface its nearest neighbours as authoritative-looking results.

Pre-tag testing on the real ob-1 vault (759 docs) showed the floor was
**mis-calibrated and failing silently** ([finding
2026-07-06](../../01-projects/onebrain/cli/2026-07-06-vsearch-floor-calibration-finding.md)
in the project vault): `vsearch` returned **0 hits** for genuine queries, and
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
   **never-silent** label from the best score: ≥0.86 confident (no hint);
   0.80–0.86 low-confidence; <0.80 "no strong match". The renderer now surfaces
   this hint on **non-empty** results too — a weak `vsearch` reads as honest
   rather than authoritative.
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

## The real fix is next (Tier 2, v3.4.x)

This ADR is the **Tier-1 stopgap**. The proper fix — as qmd (`@tobilu/qmd`) did —
is a **cross-encoder reranker** (`bge-reranker-v2-m3`, already supported by our
`fastembed-rs`, ONNX, no new dependency): retrieve recall-first, then rerank the
top-K. The reranker's calibrated 0–1 score **replaces this confidence heuristic**
as a reliable gate. Tracked as a dedicated v3.4.x epic (search is finished within
v3.4.x before v3.5). See
[search-quality research](../../01-projects/onebrain/cli/2026-07-06-search-quality-reranker-research.md).
