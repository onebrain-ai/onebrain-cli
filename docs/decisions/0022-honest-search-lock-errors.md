# 0022 — Honest search-engine lock & status errors (`E_ENGINE_BUSY`, exit 77)

- **Status:** accepted
- **Date:** 2026-07-05

## Context

`onebrain-search` stores chunk metadata in an `engine.redb` database (plus a redb-backed vector-meta db). **redb is single-process**: it takes an exclusive advisory lock on its backing file at open time, so a second process — or a second in-process handle — that opens the same collection fails with `redb::DatabaseError::DatabaseAlreadyOpen`. In OneBrain this happens routinely: the long-lived `onebrain mcp` server holds the engine open for a whole session, so any other command that opens the same collection (`search status`, `search query`, a reindex hook) hits the lock.

Before v3.4.6 that lock error was swallowed or collapsed into a generic `E_INTERNAL` / exit 1. Worst of all, `search status` under a held lock reported `doc_count: 0` and rendered **"✅ up to date"** — presenting a locked (and therefore unknown) index as a healthy empty one (bug B). The contention was invisible to both humans and machine consumers (#160).

A round-2 review of the fix found the same silent-failure shape lurking on two *non-lock* paths: when `engine.status()` fails (corrupt redb / IO / partial write), or when `Engine::open` fails for a non-lock reason (permission denied, corrupt header, missing model dir), the counts also became unknown but `busy` stayed `false` — so `render_text` fell through to the same "✅ up to date" branch over a genuinely broken index.

## Decision

- **Classify the lock case into a typed error.** `Engine::open` maps redb's `DatabaseAlreadyOpen` (matched on the typed variant, with a defensive string fallback for any path that erases it) into `onebrain_search::error::EngineBusy`. `CoreError::EngineBusy` carries it up to the CLI as a stable `E_ENGINE_BUSY` code.
- **Dedicated exit code 77.** A locked engine is transient (retry once the holder exits), semantically `EX_TEMPFAIL`. The canonical `EX_TEMPFAIL` value 75 was already taken by `E_INIT_TARGET_NOT_EMPTY`, so OneBrain uses **77** for `EXIT_ENGINE_BUSY`.
- **`search status` reports contention honestly and NEVER shows "✅ up to date" over a locked OR broken/unreadable index.** Three outcomes, all exit 0 (status is a report, not a failure):
  - **Busy** (live lock held): `busy: true`, `doc_count`/`pending_*` null (unknown, not a healthy `0`), a `W_ENGINE_BUSY` warning, text "⚠️ engine busy (indexed by another process)". Hint: retry.
  - **Unreadable** (round-2 fix — `status()` read failure, or a non-lock open failure): `busy: false`, `status_error` set to a short one-line message, null counts, a `W_STATUS_UNREADABLE` warning, text "⚠️ status read failed (…)". Hint: `search reindex --force` to rebuild. The full error chain is logged to stderr (`eprintln!("search status: {e:#}")`) so it is never swallowed.
  - **Healthy:** the "✅ up to date" / pending-drift branch, reached ONLY when counts are actually known.
- **User-facing verbs fail loudly; hook paths skip quietly.** `query` / `vsearch` / `get` / full `reindex` emit an `E_ENGINE_BUSY` envelope + exit 77. The Claude Code hook paths (`search reindex --lex-only` / `--pending-only`) instead skip with `reason: "engine-busy"` and stay **exit 0**, so a locked index never breaks the hook chain.
- **Lex `search` verb populates `heading_path`, defers `snippet`.** The lex-only verb now reads each hit's `heading_path` from the STORED tantivy field, so it needs no redb open (and so isn't subject to the lock at all). **`snippet` stays empty and is deferred**: the tantivy `body` field is indexed but NOT `STORED`, so a snippet would require a schema change plus a full reindex migration. An empty `snippet` on this verb means "not supported yet", NOT "no matching text"; the hybrid `query`/`vsearch` verbs still populate it from redb-stored chunk text.

## Consequences

- Lock contention is honest and uniform across every surface: status text/JSON, verb exit codes, and hook skips all report the same thing, and a locked or broken index can no longer masquerade as "up to date" (the exact bug B regression the round-2 fix closed).
- Machine consumers get stable, distinct signals: `W_ENGINE_BUSY` (retry) vs `W_STATUS_UNREADABLE` (rebuild) on status; `E_ENGINE_BUSY` + exit 77 on the failing verbs. `doc_count: null` unambiguously means "unknown", never a real zero.
- Exit 77 is a permanent part of the stable exit-code contract (changing it later would be a breaking change). It sits alongside the existing sysexits-style codes; 75 stays with `E_INIT_TARGET_NOT_EMPTY`.
- The lex `search` verb's `heading_path` is real, but the deferred `snippet` (and the redundant per-hit re-fetch that fills the heading today) remain follow-up work, folded into the future body-STORED schema change + reindex migration — tracked by a `TODO(v3.4.x)` in `lex::search_with_heading`. This ADR deliberately does NOT claim snippets ship in v3.4.6.
