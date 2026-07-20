# 0025 — Tier-2 cross-encoder reranker (`onebrain-rerank-v1`)

Status: accepted (v3.4.7)

> **Superseded in part by [ADR 0034](0034-heading-search-schema-selfheal-rerank-gate-decouple.md)
> (v3.4.16).** `DEFAULT_RERANK_MIN_SCORE` changed from `0.30` (below, and in the Calibration
> section) to `0.0` — the gate no longer drops hits by default — and the CLI's "no strong match"
> confidence band was split into its own constant so it no longer aliases the gate value. This
> ADR's record of the original 0.30 calibration and reasoning is left as-is below; see 0034 for
> why it changed and what stayed the same.

## Context

[ADR 0024](0024-vector-confidence-recall-first.md) retired the per-model absolute
vector floor and replaced it with a recall-first relative cutoff
(`keep_top_cluster`) plus an honest, never-silent confidence heuristic
(`vec_confidence_hint`). That ADR was explicit about its own limits: a
bi-encoder's raw cosine similarity cannot cleanly separate genuine matches
from noise for e5 on real vault content — the match/no-match score ranges
overlap. ADR 0024 named the real fix as a dedicated follow-up: a
**cross-encoder reranker**, scored per query/passage pair rather than as two
independently-embedded vectors, giving a calibrated 0–1 relevance score that
actually gates precision instead of merely hinting at it.

This ADR is that follow-up: the Tier-2 rerank stage, landing in
`onebrain-search`'s `rerank` module and wired into `Engine::query` /
`Engine::vector_search`.

## Decision

### Add a Tier-2 rerank stage after retrieval, before final ranking

Both `Engine::query` (hybrid RRF) and `Engine::vector_search` (vector-only)
now run their fused/retrieved candidates through a cross-encoder rerank stage
before truncating to `top_k`:

1. **Wide fuse** — the candidate list is fetched at
   `max(top_k, search.reranker.min_candidates)`, not just `top_k`, so the
   rerank stage always has a full pool to work with even when the caller
   asks for a small number of results.
2. **Cross-encode the reranked pool** — `max(min_candidates, top_k)` fused
   entries (the "head") are cross-encoded against the query text in one
   batch call; the remainder (the "tail", beyond that pool) is left
   unreranked and simply appended after the reranked head. See the Track E
   addendum below: `min_candidates` is a FLOOR, not a fixed window size — it
   auto-raises to cover `top_k` so every returned result is always reranked.
3. **Sort** — the reranked head is sorted by the cross-encoder's calibrated
   score, descending, with a chunk-id tiebreak for determinism (matching
   `rrf_fuse`'s and `vector::search`'s own tie-break convention).
4. **Gate, with a never-empty floor** — hits scoring below `min_score` are
   dropped, *unless* every candidate in the head would be dropped, in which
   case the top `RERANK_NO_MATCH_KEEP` (3) survive anyway. This mirrors ADR
   0024's "never empties a non-empty result set" invariant: a badly-tuned
   gate degrades to "the CLI labels these as weak," never to "the query
   returns nothing."
5. **Unreranked tail** — candidates beyond the rerank window are appended
   after the gated/sorted head, each carrying `rerank_score: None`, and the
   whole list is truncated to `top_k`.

This is a **skip-not-fail** design at every layer: no reranker configured, no
reranker downloaded, a lex-only build, or a runtime rerank error all fall
back to the plain fused order with `rerank_score: None` on every hit — a
query never fails, and never returns fewer results, because reranking didn't
work. `Hit::rerank_score: Option<f32>` is the signal callers use to tell a
reranked hit from an unreranked one; the CLI surfaces an explicit "unreranked"
hint when a semantic verb's entire result set carries no score, so a degraded
state is never silently indistinguishable from a confident one.

### Model choice: `onebrain-rerank-v1`, and what was rejected

The default (and currently only) registry entry is `onebrain-rerank-v1` — a
cross-encoder based on `bge-reranker-v2-m3`, quantized to int8, ~570 MB,
multilingual including Thai. Alternatives considered and rejected:

- **`bge-reranker-v2-m3` fp32, used directly** — the un-quantized base model
  is ~2.27 GB. That is a poor default download for a personal-vault tool
  where the reranker is meant to be an invisible quality improvement, not a
  multi-gigabyte tax every user pays on first reindex. Quantizing to int8
  keeps the same base model's relevance behavior at a size closer to the
  embedding models already in the registry.
- **`jina-reranker-v2-base-multilingual`** — technically competitive, but
  licensed CC-BY-NC (non-commercial). OneBrain ships under MIT/Apache-2.0 and
  is used commercially (Adastra-adjacent and other paid contexts); a
  non-commercial-licensed model dependency would contaminate that posture.
  Rejected on license grounds alone, independent of quality.
- **`BGE-reranker-base`** (the smaller, non-v2-m3 BGE reranker family) — no
  verified Thai support. Thai is a first-class target for this vault search
  stack (see the embed-model registry's THAI/MIRACL column in
  `docs/reference/onebrain-search.md`); a reranker that silently degrades for
  Thai content would undermine the same multilingual guarantee the embedding
  model registry already makes.

### OneBrain's own model-line naming, not a bare upstream name

The registry name is `onebrain-rerank-v1`, not `bge-reranker-v2-m3` or a raw
Hugging Face repo id. This follows the same shape as future model swaps in
the embedding registry: the model line is **versioned** (`onebrain-rerank-v1`,
`-v2`, …), each version is a **distinct registry entry**, and each entry is
**sha256-pinned** to one specific model file
(`RerankerInfo::sha256`, verified once per download via `verify_sha256_once`
and cached behind a `.sha256-verified` marker). Upgrading the underlying base
model — a new quantization, a newer BGE checkpoint, a different backbone
entirely — is always a new registry entry under a new version name, never a
silent swap of what `onebrain-rerank-v1` points at. The base-model lineage
(`bge-reranker-v2-m3`, int8) is disclosed in the registry entry's `note`
field and in this ADR, rather than hidden behind the OneBrain-branded name —
users and auditors can trace exactly what's running.

### Gating semantics: a real gate, with a recall-first floor

Unlike ADR 0024's advisory-only confidence hint, the rerank `min_score` gate
actually removes candidates from the result set — this is the "real fix"
ADR 0024 promised: a calibrated score that can be trusted to separate
relevant from irrelevant, not just to label results honestly. The
never-empty `RERANK_NO_MATCH_KEEP` floor is the one carry-over from ADR
0024's philosophy: even a real gate must not produce a bare empty result
when the alternative (the top few candidates, explicitly labeled weak) is
more useful to the user than nothing at all.

### Skip-not-fail is a contract, not an incidental default

Every failure mode in the rerank path — model not configured, not
downloaded, load failure, or a runtime scoring error — degrades to the
unreranked fused order rather than propagating an error up to the caller.
This mirrors the embedder's own lazy/skip-tolerant design
(`EmbedSource`/`Engine::embedder`) and keeps the reranker in the same
honesty posture as ADR 0024: a degraded state is always visible
(`rerank_score: None`, surfaced as an explicit "unreranked" hint) but never
fatal to the search itself.

### Download-at-reindex, not download-at-query

The reranker model downloads during `search reindex` (emitting
`ReindexProgress::LoadingReranker` at most once, at the end of a full
reindex, mirroring `LoadingModel`'s embedder-download signal) rather than on
the first query that needs it. `should_fetch_reranker(enabled, downloaded)`
is the pure decision function: fetch only when reranking is enabled in
config AND the model isn't already on disk. This keeps the query path's
"never downloads" invariant intact for `search search`/`get`/`status`, and
means a user who runs `reindex` once pays the download cost up front, at a
point where they already expect to wait, rather than as a surprise stall on
their first `search query`.

## Consequences

- **Precision actually improves**, not just gets labeled — a calibrated
  cross-encoder score is a real, empirically-groundable gate in a way a raw
  bi-encoder cosine never was (per ADR 0024's own finding).
- **Recall floor is preserved** — `RERANK_NO_MATCH_KEEP` guarantees a query
  with weak-but-present candidates still returns something, labeled
  honestly, rather than regressing to ADR 0024's exact "silently returns
  nothing" failure mode in a new disguise.
- **A second multi-hundred-MB model** joins the embedding model as an
  optional download — mitigated by quantization (int8, ~570 MB vs fp32's
  ~2.27 GB) and by deferring the download to `reindex` time rather than a
  surprise query-time stall.
- **`search.reranker.min_score`, `.min_candidates`, and the CLI confidence
  bands are provisional** at this ADR's landing — see the Calibration section
  below.
- The MCP `query` tool's qmd-compat `rerank: Option<bool>` parameter remains
  deserialize-only and inert: the native engine reranks (or doesn't)
  according to `search.reranker.enabled` in `onebrain.yml`, not per-request.
  A per-request override is a candidate future enhancement, not part of this
  ADR.

## Calibration (v3.4.7 final)

Measured on the real ob-1 vault (585 docs) with `onebrain-rerank-v1`: a
golden set of 20 answerable + 10 known-no-answer queries.

**Score separation** (the result that justifies the whole stage):

| bucket | top-hit rerank score |
|---|---|
| genuine no-answer queries | 0.003 – 0.066 (median 0.011) |
| genuine relevant matches | 0.73 – 0.99 |
| tangential / weak matches | 0.20 – 0.52 (sparse) |

Contrast e5 bi-encoder cosine (ADR 0024): relevant 0.83–0.87 vs unrelated
≈0.84 — total overlap. The cross-encoder's gap (~0.07 → 0.73) is an order of
magnitude cleaner, so its 0–1 score is a trustworthy gate where cosine never
was.

**Final constants** (the provisional values held up against real data —
measure-then-confirm, not measure-then-rubber-stamp):

- `DEFAULT_RERANK_MIN_SCORE = 0.30` — sits in the clean gap (no-answer max
  0.066 → real-match min ~0.73) with margin both sides.
- Confidence bands `0.30 / 0.60` — real matches cluster 0.73–0.99 (confident);
  no-answer all `< 0.10`.
- **`min_candidates` lowered 30 → 10** — every golden-set match already lands
  in the top ~5 after rerank, so 30 buys no quality; bge-reranker-v2-m3 costs
  ~70 ms/candidate on CPU, so 10 cuts rerank compute ~3×. User-adjustable via
  `search.reranker.min_candidates` (and per-query via `--min-candidates` /
  the API's `min_candidates` param — see the Track E addendum below).

**Latency**: warm rerank for 10 candidates is ~0.5–0.7 s (est.); the
multi-second figures seen during calibration were a *contended* daemon
(engine held by a separate MCP → per-request model reloads), not the warm
path. Cold CLI / Raspberry Pi pay the ~1.9 s model load per query and are
honestly hinted. The always-on single-owner daemon is what makes warm-load
pay off (tracked separately for v3.5).

Full report + the ship-blocker caught during this run (the model never
downloaded — a skip/fetch deadlock between the reindex and query paths) are
in the project vault.

## Addendum (v3.4.7 Track E): `min_candidates` is a floor, not a fixed window

The original wording above ("the first `candidates` fused entries... the
remainder is left unreranked") described a FIXED-SIZE reranked window: exactly
`candidates` entries, regardless of `top_k`. That was a correctness bug once a
caller's `top_k` exceeded `candidates` — with the CLI's `top_k` default (10)
matching the config default (10) this never showed up locally, but the web
service hardcoded `top_k = 20` against the same `candidates = 10` default, so
results 11–20 silently bypassed the reranker entirely on every webui search.

**Fix**: the reranked pool is `max(min_candidates, top_k)`, not
`min_candidates` alone — auto-raised whenever the caller asks for more results
than the configured pool covers. `min_candidates` is renamed from `candidates`
to make this floor semantic explicit in the name itself: it is a *minimum*
pool size, not an exact window, and it only matters when it exceeds `top_k` (a
wider pool than the return size can still improve quality by giving the
cross-encoder more to choose from before truncation). The unreranked fused
tail beyond that pool is still appended before the final truncate-to-`top_k`,
but since the pool now always covers `top_k`, that tail is always beyond the
returned set — it is dropped by truncation, never delivered.

Every surface that can request `top_k` can now also override
`min_candidates` for that one query: `search query --top-k --min-candidates`,
`search vsearch --top-k --min-candidates`, and the webui's
`/api/vault/search?top_k=&min_candidates=`. A new `search.default_top_k`
config key (default 10) gives the web service a vault-level default instead
of its old hardcoded `20`, so an unspecified request now reranks its entire
result set by default too.
