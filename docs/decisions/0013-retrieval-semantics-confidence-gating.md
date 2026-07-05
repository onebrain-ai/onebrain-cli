# 0013 — Retrieval semantics: instruction prefixes, confidence floor, exact-word lex

- **Status:** accepted; the **confidence-floor** decision is **superseded by [ADR 0024](0024-vector-confidence-recall-first.md)** (v3.4.6 — the absolute per-model `vec_floor` mis-calibrated for e5 and silently dropped real matches; replaced by a recall-first relative cutoff + honest confidence). Instruction prefixes and exact-word lex still stand.
- **Date:** 2026-07-02

## Context

Dogfooding the v3.4.0 engine on a real bilingual vault surfaced three relevance failures at once. Queries about topics the vault simply doesn't contain still returned ten authoritative-looking hits (vector search always returns its top-k nearest, however far). Thai keyword search matched documents that only shared *common character pairs* with the query (`สุขภาพ` matched anything containing `ภาพ`, because the no-space-script tokenizer ORs character bigrams). And the e5 model family was embedded raw, although it is trained with instruction prefixes — omitting them measurably degrades retrieval, which compounded both problems.

## Decision

Three coupled rules define what a "hit" means:

1. **Model-aware instruction prefixes.** The registry carries `query_prefix`/`passage_prefix` per model (`"query: "`/`"passage: "` for the e5 family; empty for bge-m3; Gemma's documented prompts for the gemma variants). Applied only at embed time — stored chunk text stays raw.
2. **Vector confidence floor.** The registry carries `vec_floor` per model (0.85 for the e5 family — measured on the real vault: unrelated text clusters ≈0.84, real matches ≥0.87). Candidates below it are dropped *before* RRF fusion, in both `query` and `vsearch`. Models without a measured floor (bge-m3, gemma) have none until calibrated. A `--min-score` flag gives manual control.
3. **Exact-word lex for no-space scripts.** Each script run (pseudo-word) in a query becomes a nested Boolean requiring **all** of its bigrams: a lex hit means the document really contains the queried word. Substring queries still work (only the *query's* bigrams are required); fuzzy/semantic recall is the vector side's job. Spaced-script words remain plain OR terms under BM25 IDF.

## Consequences

- "No results" is now a truthful answer; junk stops outranking silence. The cost is a hard dependency on per-model calibration — an uncalibrated floor either lets noise back in (`None`) or eats borderline cross-lingual matches (too high). Floor values are registry data, not code, so recalibration is cheap.
- Changing prefixes (or anything that alters embedding geometry) silently invalidates every stored vector — the content-hash diff cannot see it. `search reindex --force` (wipe + rebuild, models kept) exists for exactly this class of change.
- Thai lex is intentionally stricter than a web search engine: no partial-word fuzziness. The accepted follow-up is dictionary word-segmentation (nlpo3), not loosening the bigram rule.
