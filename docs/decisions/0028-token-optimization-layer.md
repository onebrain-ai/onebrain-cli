# 0028 — Token optimization layer: `onebrain-token` crate, level ladder, honesty contract

- **Status:** accepted
- **Date:** 2026-07-11

## Context

Every search hit, note body, and memory load OneBrain hands to an agent spends the agent's context window. The MCP `get`/`multi_get` path inlines whole documents (real vault notes reach ~15k tokens); `query` returns per-hit JSON with repeated envelope keys; nothing measures what any of it costs. Generic output filters (RTK-style proxies) can only reshape output that passes through them — OneBrain owns the producer, so it can be both smarter and safer. But the presentation layer is duplicated today: MCP's `QueryHit` and the CLI's `HitData` are different shapes on different code paths, so any per-surface fix would drift.

v3.4.10 ships the optimization layer whole (deferred from v3.4.8/v3.4.9 planning): lossless and lossy techniques, adjustable, measured.

## Decision

- **New crate `onebrain-token`** holds all transforms as pure functions (no I/O), gain accounting, and the cache schema. MCP, the daemon's HTTP responses, and the CLI search verbs all call the same shaping code via a small shared trait — struct shapes and field names stay surface-local (no breaking renames).
- **One ordinal knob, four rungs**, each a superset of the previous: `off` (0) → `conservative` (1, default — lossless only: JSON compaction, cross-hit doc dedup, whitespace compaction, generous `get` continuation cap) → `balanced` (2, + frontmatter strip, tighter cap, **already-sent ledger on**) → `aggressive` (3, + snippet-less query, `multi_get` head-only). Per-call override (`opt_level` / `--opt-level`) beats config beats default.
- **Honesty contract (non-negotiable):** every lossy transform emits a machine-readable signal — `truncated` + continuation cursor, `snippet_omitted`, `chunks_collapsed`, or a reference envelope with a re-materialize instruction. No silent drops. The optimizer runs after rerank/fusion and never reorders or removes hits, only compacts representation.
- **Single runner funnel:** every agent-facing response passes one path — transform → `never_worse` guard (if the transformed payload estimates larger than the original, emit the original) → gain event record. A surface cannot bypass metering by construction; an integration test per surface asserts a gain event lands.
- **Transform guard test:** every registered transform ships fixture tests captured from real output; a guard test enumerates registered transforms and fails if any lacks fixtures (same enforcement pattern as `every_config_struct_key_has_a_doc_entry`).
- Human TTY text output is untouched this release; agent-facing output only (`--json` modes, MCP, daemon routes).

## Consequences

- Default users get strictly-lossless savings with zero accuracy risk; heavy sessions opt into lossy rungs knowingly, and every reduced view is recoverable.
- The shared crate means one implementation to test and one place future transforms land — at the cost of a trait seam between three surfaces that must be kept honest (the surface-coverage matrix in the v3.4.10 design is the checklist).
- `never_worse` and the funnel add a comparison and a log write to every response — accepted overhead, microseconds against multi-ms search calls.
- The `token` command is a new first-level CLI command shipping in a patch release — a logged exception to the "minor = new first-level command" rule, accepted because token-opt closes the v3.4 "Native Search" theme and v3.5 is already reserved.
