# 0030 — Gain telemetry: raw JSONL keep-everything + precomputed rollups + epoch reset

- **Status:** accepted · updated by #283 (reads are JSONL-first; rollups = `--rebuild`-only)
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

## Update — #281/#283 (v3.4.14): reads are JSONL-first; rollups are legacy

The "all reads serve from rollups" decision above never became true in practice:
the incremental daemon-side rollup write ("same write batch as the raw append")
was not built, and v3.4.12 (#258, ADR 0032) moved recording + the default CLI
reads to the lock-free JSONL to survive daemon lock contention. That left the
rollup DB permanently empty on any fresh cache — `--rebuild` (its only writer)
cannot run while a daemon holds `token.redb` — so the daemon's
`GET /api/token/gain` (the WebUI dashboard) and `--all-time`/`--since` reported
zeroes forever (#281, the same failure class as #257).

Corrections, shipped in #283:

- **Every read serves from the raw JSONL** through the one `query_events` pivot
  engine: the CLI default (current epoch, `read_all`), `--all-time`/`--since`
  (all epochs, `read_all_recursive` including `gain/archive/**`), and the
  daemon route the WebUI consumes. Daemon-routed and Direct answers agree
  because they read the same files; a routing failure (404/transport) falls
  through to the lock-free Direct read, eliminating the `E_ENGINE_BUSY`
  catch-22 on all-epoch reads.
- **Rollups are legacy:** `GAIN_DAILY`/`GAIN_MONTHLY`/`GAIN_YEARLY` and
  `pivot::query` have exactly one remaining user — `token gain --rebuild` —
  pending removal in a follow-up.
- **The flat-read-latency property is traded away** for truthfulness: gain
  reads now walk the JSONL per request, so read cost grows with history length
  until log compaction or read caching lands (follow-up).
- The route keeps a **legacy-bare compatibility rule**: a request with neither
  a `since` nor an `all_time` key is a pre-3.4.14 CLI's `--all-time` and is
  served all-epoch; the webui always sends `?by=&since=` (keys present, empty
  = unset) and gets the current epoch.
