# 0032 — Self-healing daemon fallback: token gain, serve, and vsearch under redb contention

- **Status:** accepted
- **Date:** 2026-07-12
- **Supersedes (in part):** [0023](0023-warm-daemon-mcp-search.md) (the "`vsearch` is not daemon-routed" mapping) and the `serve` "never starts/stops/restarts a daemon" stance.

## Context

redb is single-process: exactly one process may open `engine.redb` / `token.redb` at a time. A warm `onebrain daemon` (or `onebrain mcp`) holds those locks for its whole lifetime. Post-ship testing of the token-optimization epic (v3.4.10/11) surfaced that several surfaces still hard-errored, failed to run, or served degraded results whenever a daemon was (or wasn't) holding a lock — the opposite of "runs smooth" (#258, #257):

- `onebrain token gain` opened `token.redb` eagerly, so **every** mode errored `Database already open. Cannot acquire lock.` under a running daemon.
- `onebrain serve` with no daemon ran an engine-less foreground standalone — no token cache, so the Token-Gain dashboard was dark (#257); against a different-version daemon it hit a port/lock conflict.
- `onebrain search vsearch` had no daemon route, so it returned `E_ENGINE_BUSY` while an mcp session held the engine.

Most surfaces already self-healed (search `query`/`get`/`reindex` Direct-fall-back + `with_retry` respawn; `mcp` auto-starts via `ensure_running`; `token check` fails open). These three were the gaps.

## Decision

- **Two-tier reads bypass the lock.** `token gain`'s default summary / `--by` / `--history` / `--reset` read the lock-free JSONL raw log (Tier-1 source of truth, ADR 0030); only `--all-time` / `--since` / `--rebuild` touch the redb rollup. The rollup DB is opened **lazily**, only in the branches that need it.
- **Read verbs route across version skew, never restart.** `token gain --all-time/--since` and `search vsearch` route to a **same-vault** daemon regardless of its version (`discover_same_vault_any_version` / passive `route_to_daemon`), because the routes they use return version-stable shapes (`PivotResult`, `SearchHit`). This closes the upgrade-without-restart gap: right after a CLI upgrade the still-running old daemon holds the lock, and the read still works without the user restarting it. A CLI **read** verb must never stop/restart another session's daemon — that stays `ensure_running`/`mcp`/`serve`'s job.
- **`serve` is an active lifecycle owner: reuse-or-start.** `serve` reuses a matching daemon, else **starts** one (restarting a stale/version-mismatched one) via `ensure_running`, then hands over that daemon's URL. The started daemon holds the engine + token cache, so the dashboard is populated. The explicit `--port`/`--dir`/`$ONEBRAIN_BIND` escape hatch still runs a foreground standalone — which now also opens its own token cache (`ServeConfig.open_token_cache`) so its dashboard isn't dark either.
- **New `vec` daemon mode.** `GET /api/vault/search?mode=vec` runs `Engine::vector_search` against the held (or per-request) engine, mirroring the `hybrid` path; `search vsearch` routes through it.
- **One transient-lock signal.** A genuinely contended open (no daemon to route to, or a too-old daemon) maps to `CoreError::EngineBusy` → `E_ENGINE_BUSY` / exit 77 — the same code `query`/`vsearch`/`get` already use — with an actionable "retry after `onebrain daemon stop`" hint, never a raw redb error at exit 1.

## Consequences

- `serve` can now restart the machine's single daemon (e.g. to replace a version-mismatched one), which briefly disrupts a cross-vault mcp session bound to that daemon — the session self-heals via its own `with_retry` reconnect. This is the accepted trade for "serve always runs and its dashboard is populated"; true multi-vault daemons are the #230 follow-up.
- `vsearch`'s daemon path uses the generic unreranked hint rather than the direct path's cosine-confidence fold-in (advisory text only; hit data + ordering are identical). A per-query `min_score` on the daemon path can only tighten past the daemon's config floor — identical to the already-shipped `query` daemon path.
- The hit-mapping closure is now duplicated four ways (`run_hybrid`/`run_hybrid_held`/`run_vec`/`run_vec_held`); extraction is a low-priority follow-up.
