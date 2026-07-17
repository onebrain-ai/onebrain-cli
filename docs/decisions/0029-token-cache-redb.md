# 0029 — Token cache on redb: memoization + already-sent ledger + generation counter

- **Status:** accepted · updated by [#283](https://github.com/onebrain-ai/onebrain-cli/pull/283): gain reads are JSONL-first since v3.4.14 — the rollup-read rationale in §(d) applies only to the `--rebuild` leg
- **Date:** 2026-07-11

## Context

The token-optimization layer (0028) needs persistent state: repeated identical queries should skip embedding + cross-encoder rerank (~70 ms/candidate on CPU), and a document already delivered to an agent session should not be re-inlined at ~15k tokens when it hasn't changed. Both need storage that is fast for point lookups, safe under the daemon's ownership model, and honest about staleness. RTK — the closest prior art — uses SQLite; the obvious question was whether to follow.

## Decision

- **Storage = `token.redb` under `<collection_cache>/token/`**, a new sibling of `models/` and `index/`. `CollectionLayout` migration/detection scans only `models--*` prefixes and the three index artifact names, so the new dir collides with nothing (verified against 0027's resolver).
- **redb, not SQLite.** The deciding facts: (a) the warm daemon is already the sole long-lived redb owner (0023) — a single-writer store fits the architecture we have, whereas RTK has no daemon and *needs* SQLite's WAL to survive many short-lived processes writing concurrently; (b) redb is already a dependency — SQLite would bundle a C library into the pure-Rust binary; (c) our hot path is exact-key point lookup (hook budget <200 ms including HTTP), not ad-hoc SQL; (d) gain reporting reads precomputed rollups (0030), so read-time `GROUP BY` — SQLite's one real advantage here — goes unused. In `Backend::Direct` (no daemon), the process opens `token.redb` itself; safe because Direct mode only exists when no daemon holds it.
- **Query memoization:** key = SHA-256 of `(normalized query, mode, top_k, min_candidates, min_score, index_generation)`; value = the resolved hit set. A **new `generation` key in `ENGINE_HEADER`** is bumped in the same write transaction on **every** reindex — full and lex-only alike (`last_indexed_at` is full-mode-only and cannot serve; the PostToolUse lex-only hook fires constantly and must invalidate). Staleness is impossible by construction: the generation lives in the key, so old entries simply stop matching.
- **Already-sent ledger:** key = `(session_token, doc_path)` → content hash last delivered. Current hash equal → return a small reference (`sent_earlier`, hash, `bytes_saved`, re-materialize instruction) instead of the body; hash differs → deliver fresh content and update. The client resolves `session_token` (existing `resolve_session_token`) and attaches it to every daemon call — one daemon serves many sessions. Unresolvable token → ledger inactive for that call, inline as normal. Ledger activates at level `balanced`+ only. Entries are timestamped and pruned opportunistically (default 7 days).
- **`--force` re-materialize:** `get`/`search get` gain a `--force` that bypasses ledger and caps — always full content. Every reference embeds this instruction, closing the "agent compacted its context and lost the doc" hazard without waiting for deeplinks.
- Content hashes come from the engine's stored `DOC_HASHES` (kept fresh by the reindex hooks) via a new public accessor — no re-hashing of large documents on the read path.

## Consequences

- Repeat work — the bulk of a long agent session — stops being paid for twice: repeated queries return instantly, repeated reads cost a ~50-token receipt instead of ~15k.
- The whole cache is derived state: delete `token/` and everything regenerates; nothing user-authored lives there.
- Cross-process invalidation rides on machinery that already exists (hashes, the daemon, hooks) — the one new invariant to defend is "every reindex path bumps `generation`", pinned by a dedicated test.
- We diverge from RTK's storage choice deliberately; if a future need for ad-hoc analytical queries over raw history appears, the answer is the JSONL event log (0030), not a SQL engine in the binary.
