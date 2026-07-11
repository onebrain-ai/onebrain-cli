# 0030 — Gain telemetry: raw JSONL keep-everything + precomputed rollups + epoch reset

- **Status:** accepted
- **Date:** 2026-07-11

## Context

"OneBrain uses less of your context than anything else" is only a real claim if it's measured. The measurement store must answer per-call questions ("what exactly happened"), period questions ("this month vs last"), pivot questions ("savings by level × surface"), and baseline experiments ("reset, run a week at `balanced`, compare") — and it must feed a web dashboard without recomputing history on every view. RTK's approach (SQLite, aggregate-at-read, 90-day hard delete, `--reset` = `DELETE FROM`) fails the keep-everything and baseline requirements outright.

## Decision

- **Tier 1 — raw events, kept forever:** `token/gain/YYYY-MM.jsonl`, one line per optimized call: `{ts, surface, transform, level, bytes_before, bytes_after, cache: memo_hit|ledger_ref|none, session_token}`. Append-only JSONL: no transaction cost, crash-tolerant, greppable by a human. Monthly rotation bounds file size; nothing is ever aged out.
- **Tier 2 — precomputed rollups:** redb tables `GAIN_DAILY` / `GAIN_MONTHLY` / `GAIN_YEARLY` in `token.redb`, key `(period, surface, transform, level, cache_kind)` → `{bytes_before, bytes_after, count}`, updated incrementally in the same daemon write batch as the raw append. **All reads — CLI summary, pivots, and the Web API — serve from rollups in constant time regardless of history age.** Only `--history` tails the raw log.
- **Rollups are disposable:** `onebrain token gain --rebuild` reconstructs them from raw JSONL; `doctor` checks rollup↔raw drift. The JSONL is the single source of truth.
- **Reset = epoch cut, never deletion:** `--reset [--label <name>]` archives the current window to `gain/archive/<ts>-<label>/` and starts fresh. This is the baseline-testing mechanism: reset → run at a level → read → reset → switch level → compare. (The `level` field on every event also allows mixed-window comparison via `--by level,…` without any reset.)
- **Reporting surfaces:** `token gain` summary (session/today/month/all-time + per-surface bars), `--by <time,dim>` pivots (day|week|month|year × surface|transform|level|cache), `--since`, `--json`; daemon `GET /api/token/gain` returns the same pivot JSON for the WebUI dashboard (and Studio when it wraps the WebUI). One pivot engine in `onebrain-token` serves all three — CLI verbs go through the standard `output::emit` envelope, never a hand-rolled printer.
- **Estimates labeled as estimates:** bytes→tokens uses a calibrated bytes-per-token table per model family, calibrated on real markdown and Thai samples — explicitly avoiding RTK's `len()/4` byte-count heuristic, which systematically over-counts multi-byte UTF-8. Dollar figures appear only when a model is declared.

## Consequences

- Every question is answerable forever (raw is never discarded), yet report latency is flat forever (reads never touch raw) — the cost is double-write bookkeeping and the drift check, both cheap and self-healing via `--rebuild`.
- Baseline experiments become a first-class workflow instead of a data sacrifice.
- Storage grows unbounded by design (~2–5 MB/month heavy use, plain text) — accepted per the keep-everything requirement; archives compress well if it ever matters.
