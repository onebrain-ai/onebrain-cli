# 0034 — Heading search enables the lex schema; self-heal over a hard error; rerank gate decoupled from the confidence band

- **Status:** accepted
- **Date:** 2026-07-19
- **Follows:** [0025](0025-tier2-cross-encoder-reranker.md) (Tier-2 cross-encoder reranker — this ADR changes two of its constants) and [0027](0027-collection-cache-layout-split.md) (the precedent for eager, invisible migration on `Engine::open`).

## Context

`chunk.rs` has always computed `heading_path` (`"A > B > C"`) per chunk and `lex.rs` has always stored it in tantivy alongside `body`. But it took no part in retrieval: `LexIndex::build_query` only ever targeted `body`, so a query matching a document's heading text but not its body text simply did not match. Worse, `heading_path` was declared plain `TEXT` — tantivy's default English tokenizer — while `body` used the script-aware tokenizer (`SCRIPT_AWARE_TOKENIZER`) added for Thai/Lao/Khmer/Myanmar/CJK. Even a query-side fix to search `heading_path` would have been useless for non-Latin headings, because the field itself had never been tokenized in a way that could match them.

Fixing this requires two changes that can't be made independently:

1. **Query-time**: build every query against both `body` and `heading_path`.
2. **Index-time**: give `heading_path` the same script-aware tokenizer as `body`.

Change 2 alters the tantivy schema. Tantivy refuses to open an index whose on-disk schema doesn't match the one the code declares (`Schema error: 'An index exists but the schema does not match.'`) — so every vault's existing `tantivy/` index becomes unopenable the moment this ships, unconditionally, on the very next `onebrain search`/`query`/`vsearch`/reindex/hook call after upgrade.

A second, independent problem surfaced while calibrating the heading boost on a real 782-doc vault: the rerank gate (`DEFAULT_RERANK_MIN_SCORE`, [ADR 0025](0025-tier2-cross-encoder-reranker.md)) was silently deleting a large fraction of correct heading and keyword hits — see the Decision section below.

## Decision

### 1. `heading_path` becomes searchable, with a below-1.0 boost

`heading_path` is re-declared with the script-aware tokenizer (matching `body`), and `LexIndex::build_query` now builds every query unit against **both** fields, with the heading side wrapped in `BoostQuery(HEADING_BOOST)`.

`HEADING_BOOST = 0.35`, calibrated on a real 782-doc vault (BM25 top-10, fixed seed, two 30-query probe sets — **A**: queries drawn from a heading occurring in exactly one file; **C**: distinctive body-term queries, a regression guard against hurting the already-working case):

| boost | A hit@10 | A MRR | C hit@10 | C MRR |
|---|---|---|---|---|
| 0.00 | 0.300 | 0.185 | 0.800 | 0.5667 |
| 0.35 | 0.600 | 0.428 | 0.800 | 0.5469 |
| 0.75 | 0.733 | 0.631 | 0.700 | 0.4770 |

0.35 is the knee: heading recall doubles (hit@10 0.300 → 0.600) with the body-term set's hit@10 completely untouched. Past 0.35, the body set starts losing hits outright (0.75 drops C's hit@10 to 0.700). The boost is deliberately kept **below** 1.0 — an equal or greater weight would let a heading match dominate a body match — because a chunk's `heading_path` is identical across every chunk under that section: an over-weighted heading term floods the top-k with every sibling chunk of a section whose title happens to contain the query word. This is a real, not hypothetical, hazard in a vault where hundreds of session logs share boilerplate section headings ("Key Decisions", "Action Items").

### 2. Self-heal the schema break instead of returning an actionable error

The alternative to self-healing was a hard error on open, pointing the user at `search reindex --force` (or an equivalent explicit re-index command). That was rejected: `--force` re-chunks **and** re-embeds every document — reloading or downloading the embedding model, re-running the whole ONNX pipeline — to fix a problem that has nothing to do with the vectors or the chunk boundaries. Every chunk's text is already sitting, untouched, in `engine.redb`'s `chunk_meta` table; only the lex (BM25) index needs to change.

So the schema break self-heals instead: `LexIndex::open_or_reset` catches specifically a **typed** schema mismatch (`tantivy::TantivyError::SchemaError`, matched via `is_schema_mismatch` — never a raw I/O or permission error, which propagate untouched) and wipes + recreates the tantivy index at the new schema. `Engine::open` then calls `repopulate_lex_from_meta`, which walks every `chunk_meta` row and re-adds each chunk to the freshly emptied lex index — no vault file is re-read, no chunker runs, no embedder is constructed, and the vector store and `doc_hashes`/`lex_hashes` are untouched, because the underlying content never changed.

The read-only lex fast paths that intentionally bypass `Engine::open` and redb entirely (`search search`, the MCP `lex` sub-query) can't self-heal on their own — refilling the lex index needs `chunk_meta`, which only the engine can reach. They route through a new seam, `open_lex_migrating`: on a plain open they behave exactly as before; on a schema mismatch they open the engine once (which self-heals via `open_or_reset` + `repopulate_lex_from_meta`), drop it to release the collection lock, and retry the plain open. Migration is therefore invisible on every surface, not only the engine-backed ones.

Measured on the real 782-doc vault: **1.26 s** for the full self-heal on first call after upgrade, **8 ms** on every call after (the schema now matches, so `open_or_reset` takes the fast path). A migrated index scores identically to one built fresh at the new schema — the repopulation reconstructs the exact same lex documents `reindex_all` would have produced, just without the chunking/embedding work.

This follows the same shape as [ADR 0027](0027-collection-cache-layout-split.md)'s eager-migration precedent: the user never runs a "migrate" command, the first post-upgrade touch of a collection converts it, and the cost is bounded (there, a rename; here, a redb table scan — never a re-embed).

### Crash safety during migration

A migration interrupted mid-flight (process killed between wiping the old tantivy index and finishing repopulation) must not leave a collection permanently broken. That guarantee — an interrupted migration is detected and retried on the next open, rather than being mistaken for "already migrated" or silently left half-populated — is being added concurrently in this release and is described here only by its observable behavior, not its implementation: callers can rely on a killed migration self-correcting on the next `Engine::open`, with no manual recovery step.

### Downgrade is NOT supported

Once a collection's tantivy index has migrated to the script-aware `heading_path` schema, opening that same collection with a pre-3.4.16 binary fails, and fails in ways that don't clearly say why:

- `search search` errors with a bare `E_INTERNAL` — the schema-mismatch cause is not surfaced by the old binary, because it predates `is_schema_mismatch`.
- `search status` reports `indexed: false` — misleading, since the index is in fact complete and correct for the new binary; the old binary just can't open it to count.
- `reindex` and `reindex --lex-only` both refuse to run against the mismatched schema.
- Only `reindex --force` on the **new** binary (a full re-embed) recovers a collection that somehow needs to move back — there is no lightweight downgrade path, unlike the ADR 0027 layout split, which tolerated a mixed-version window because both layouts were mutually resolvable. A schema mismatch is not: tantivy either opens an index or it doesn't.

Operationally this means: don't downgrade the CLI binary below v3.4.16 once a vault's search collection has been touched by it. This mirrors the existing "one machine, one CLI version" assumption from ADR 0027, made stricter here because there is no partial-compatibility middle ground.

### 3. `DEFAULT_RERANK_MIN_SCORE`: 0.30 → 0.0

[ADR 0025](0025-tier2-cross-encoder-reranker.md) set `DEFAULT_RERANK_MIN_SCORE = 0.30`, calibrated on 30 **question-shaped** queries where genuine matches scored 0.73–0.99 and non-matches scored 0.003–0.066 — a clean, wide gap. That calibration held for the query shape it was measured on and silently failed for a shape it wasn't: **keyword and fragment queries**, which score inside the gated band even when correct — exactly the shape the agent's `lex` sub-queries produce, and exactly the shape a heading lookup produces now that heading search exists.

The gate's actual failure mode is not what ADR 0025 assumed. That ADR's design reasoned the never-empty floor (`RERANK_NO_MATCH_KEEP = 3`) "backstops total rejection" — true, but total rejection was never the danger. The real damage is **partial** rejection, which nothing backstops: with `candidates` and `top_k` both defaulting to 10, there is no unreranked fused tail left over once the gate has removed some (not all) of the reranked pool — the gate simply deletes those rows outright.

Measured on the same real 782-doc vault (60 probes): the 0.30 gate cut heading-shaped hit@10 from 0.500 to 0.233, and body-term hit@10 from 0.733 to 0.500 — **half the correct answers removed**.

`DEFAULT_RERANK_MIN_SCORE` moves to **0.0**. At 0.0 the cross-encoder still does its real job: it **reorders** the candidate pool by calibrated relevance, putting strong matches on top. It stops doing a second job it was never well-suited to for this query shape: deleting rows. Every hit still carries its `rerank_score`, which is exactly what the search cascade (`skills/startup/SEARCH.md`) already instructs the agent to judge confidence on. Raising `search.reranker.min_score` above 0.0 remains available as an explicit, opt-in way to re-enable hard filtering — `RERANK_NO_MATCH_KEEP` still guards total rejection in that case.

### 4. The confidence band is decoupled from the gate

Before this change, `RERANK_NO_MATCH_BAND` in `search_query.rs` **aliased** `DEFAULT_RERANK_MIN_SCORE` outright — the two were declared to "never drift apart," which was safe reasoning only while the gate actually filtered: a hit scoring below the gate could only reach the reader via the never-empty floor, so gate value and "this is a weak result" label were necessarily the same number.

Dropping the gate to 0.0 without touching the band would have collapsed the "no strong match" band to nothing — every weak hit would relabel itself "possible match," deleting the honesty signal instead of the rows it used to delete. That is a worse regression than the one this ADR fixes.

The band is now its own literal constant, `RERANK_NO_MATCH_BAND = 0.30` — the same numeric value ADR 0025 calibrated, but no longer sourced from `DEFAULT_RERANK_MIN_SCORE`. The two constants now answer genuinely different questions and are allowed to diverge:

- **The gate** (`DEFAULT_RERANK_MIN_SCORE`) asks *should this row be deleted from the result set?* — now: never, by default.
- **The band** (`RERANK_NO_MATCH_BAND`) asks *how much should the reader trust this row?* — still: below 0.30 means "no strong match," per ADR 0025's original calibration on question-shaped queries.

## Consequences

- **Heading search works, in every supported script.** A query matching only a document's heading — including Thai/CJK headings, which could never have matched before even if `build_query` had targeted `heading_path` — now surfaces that document.
- **Every existing vault's search collection self-heals on first post-upgrade touch, invisibly.** No user action, no re-embed, no model download; a one-time cost of ~1.26 s (782-doc vault) borne by whichever surface happens to open the collection first.
- **Downgrade below v3.4.16 is a hard break**, not a graceful degrade, for any collection this version has touched — see the Downgrade section above. This is stricter than the layout-split precedent (ADR 0027) and needs to be called out prominently anywhere upgrade/downgrade is documented.
- **Rerank no longer deletes results by default** — `query`/`vsearch` now normally return the caller's full requested `top_k` (modulo the never-empty floor's own edge cases), reordered by relevance rather than filtered by it. A vault operator who wants hard filtering back opts in via `search.reranker.min_score`.
- **The confidence band survives the gate change**, so the search cascade's `< 0.30` / `0.30–0.60` / `≥ 0.60` trust bands (used by the agent to judge which hits to believe) keep working exactly as before — the band was never the thing that was wrong.
- Two independent fixes ship in the same release because they were discovered together, calibrating the same boost on the same real vault: the heading-boost work is what surfaced the rerank gate's keyword-query blind spot in the first place.
