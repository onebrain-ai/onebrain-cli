# 0023 — Warm daemon owns the search engine for mcp + CLI search

- **Status:** accepted
- **Date:** 2026-07-05

## Context

The native-search engine (ADR 0021) stores its metadata in `engine.redb`, and **redb is single-process / single-writer**: exactly one process may open a given collection's database at a time. A second opener fails with `Database already open`.

Today every search surface opens its own engine per use:

- `GET /api/vault/search` (webui) opens `Engine`/`LexIndex` **per request** (`server/search.rs`).
- The CLI `onebrain search …` verbs open an engine per invocation (`search_common::open_engine`).
- The native MCP server (`commands/mcp.rs`) opens one for its `query`/`status` tools.

As long as only one runs at a time this is fine. But the product direction is multiple concurrent sessions (agent-teams, the webui, and CLI all live at once), and the reindex-on-write hook fires from whichever session just wrote a note. Two of those opening the vector engine simultaneously hit redb's single-writer limit. We need exactly ONE long-lived engine owner, with the other surfaces talking to it.

An existing HTTP surface already runs in-process: `onebrain serve` / `daemon __run` share `crate::server` (an Axum app on `127.0.0.1` behind a per-session token). The daemon (`daemon start`/`stop`/`status` + the detached `__run` body) already exists but opened the engine per-request like everyone else and had no concurrent-start guard.

## Decision

**The warm daemon becomes the sole redb owner; mcp + CLI search become HTTP clients of it.** Transport is the **existing localhost Axum HTTP surface + the existing token auth** — no new IPC mechanism.

Concretely, this PR (v3.4.6 Track 2a) builds the **daemon side + the reusable client library**; the mcp + CLI client wirings are separate tracks.

1. **Persistent engine.** `daemon __run` opens the engine ONCE at boot and holds it in `Arc<Mutex<Engine>>` (`server::SharedEngine`) on `AppState.search_engine` for the process lifetime. `GET /api/vault/search` uses the held engine (hybrid) instead of opening per-request; lex stays on the standalone `LexIndex` (tantivy only — it never touches redb, so it can't clash). `serve` and the unit-test router leave the engine unheld (`hold_engine: false`) and keep the per-request behaviour, since a foreground `serve` is short-lived and not the canonical owner.

2. **Internal endpoints** (token-gated, same middleware as the rest of the surface):
   - `POST /api/internal/reindex` — body `{ "mode": "pending" | "paths", "paths"?: [..] }`. `pending` embeds exactly `Engine::pending_vector_paths` (mirrors `search reindex --pending-only`); `paths` runs `Engine::reindex_paths` over caller-supplied doc paths. **Every `paths` entry is confined to the vault at the HTTP layer** (`api::confine_reindex_path`) BEFORE it reaches the engine — absolute paths, `..` traversal, symlink-escapes, and tooling dirs are rejected with 400, so a caller can't steer `vault_root.join(doc_path)` + `std::fs::read` outside the vault. Writes serialise on the engine mutex.
   - `GET /api/internal/status` — the real `{ doc_count, pending_new, pending_changed, pending_removed, pending_total, last_indexed, indexed }` from the held engine (the `search status` shape).
   - `GET /api/health` — an **engine-INDEPENDENT** liveness route: 200 (`{ ok, engine_held }`) whenever the process is up, regardless of whether an engine is held. This is what the client's `is_live()` probes. The `/internal/*` routes 503 when no engine is held (internal callers depend on the single-owner invariant and never fall back to a per-request open), but liveness MUST NOT depend on the engine — see the `is_live` fix below.

3. **Discovery** — the daemon publishes `~/.onebrain/run/daemon.json` = `{ port, token, pid, version }` after it binds (0600, atomic temp-then-rename) and removes it on clean shutdown. `port` is the ACTUAL bound port (matters when binding `0`); `version` is the daemon CLI's own `CARGO_PKG_VERSION`.

4. **Concurrent-start guard** (was missing) — `daemon start` takes an exclusive `O_EXCL`-create lock (`daemon.lock`) around the check-then-spawn window. The lock is **self-describing**: it records the creating `daemon start` PID, and a contender reclaims it ONLY if that PID is provably dead (a crashed starter) — a live creator, or an unreadable PID, is respected. Critically, the reclaim keys off the LOCK's PID, NOT the daemon PID file (which isn't written yet during a fresh concurrent start — that was the initial bug: a daemon-PID probe read "not running" and every racer reclaimed + spawned). The winner ALSO holds the lock until the daemon has published `daemon.json` (fully bound), so serialized racers always observe a running daemon and back off. Result: N simultaneous `daemon start` → exactly one daemon. `create_new` maps to `O_EXCL`/`CREATE_NEW`, so this is cross-platform — no `flock`, no nix `fs` feature.

5. **Client library** (`commands/daemon_client.rs`, reusable): `DaemonInfo` (read/write/remove of `daemon.json`); `discover()` (read + `/api/health` liveness-probe, clean up stale/dead); `ensure_running()` (spawn `daemon start` detached, poll for readiness, handle the start race by connecting to whoever won; on timeout it CLASSIFIES the last state — not-found / version-skew / alive-no-response — and names the `daemon.log` path); `DaemonHandle::search`/`reindex`/`status` (typed ureq calls carrying the token, each **retrying ONCE via `ensure_running` on a TRANSPORT error** — a vanished daemon — while an HTTP status error propagates unchanged); and version-skew handling (`version_decision` → stop + restart when `daemon.json.version != own`; a stop FAILURE is surfaced as a warning, not swallowed).
   - **`is_live` fix (silent-failure).** `is_live()` probes `/api/health`, NOT `/api/internal/status`. Probing status would 503 for a live-but-engine-less daemon → the client would read it as dead, delete its `daemon.json`, and respawn it in a loop. The health route is engine-independent, so a running daemon is always seen as alive and its discovery record is preserved.

6. **Lifecycle** — idle-shutdown TTL (`$ONEBRAIN_DAEMON_IDLE_SECS`, default 30 min; `0` disables), driven by an `AppState.last_activity` marker the auth middleware bumps on every authenticated request (predicate `should_idle_shutdown`); and clean SIGTERM handling that drops the engine (releasing the redb lock) and removes `daemon.json`. `daemon stop` ALSO removes `daemon.json` (both branches), so a hard-killed daemon's stale record is cleared on the next `stop`; any remaining stale record is reclaimed lazily by `discover`'s liveness probe.

### High-assurance coverage (repo-owner policy)

Security/filesystem-touching daemon code is **NOT whole-file-excluded** from coverage. `daemon.rs`, `daemon_client.rs`, and `server/search.rs` were removed from the exclusion allowlist; their LOGIC (concurrent-start orchestration + lock reclaim, path confinement, `is_live`/health decision, discovery + version-skew, the retry policy, the idle-shutdown predicate) is unit-tested toward the target, plus live-server HTTP round-trips and a real-binary concurrent-start orchestration test. Only the irreducible OS/network/embed shell (fork/detach, bind, signal-wait, real embedding) remains, documented per-line under "Residual unreachable lines" in `docs/coverage.md`. The un-exclusion pulls that shell into the measured surface, so the ratchet was reset 95 → 94 (achieved ≈94.99%) as a deliberate new baseline — see `scripts/coverage.sh`.

### Rejected / deferred

- **Bespoke unix-domain socket — REJECTED (deferred to the v3.8 full daemon refactor).** A unix socket is arguably a tighter fit for a local-only IPC, but it isn't cross-platform (Windows named pipes would need a parallel path) and would mean a *second* server stack alongside the Axum one we already run for the webui. Reusing the existing HTTP surface + token auth gives cross-platform behaviour and maximum code reuse now; a socket can be revisited when the full daemon refactor consolidates all surfaces (v3.8).
- **Daemon owning ALL surfaces (config/tree/file/chat routed through it) — OUT OF SCOPE.** This PR scopes the daemon to the **search** engine ownership problem (mcp + search). The remaining surfaces still work per-request; consolidating them is the later full-refactor cleanup.
- **HTTP-layer doc-count-rise test with a real embed — avoided.** Asserting `reindex {pending}` raises `doc_count` requires embedding, which requires a model download; the no-download rule forbids that in the test suite and no stub model exists in-repo. Engine reindex correctness is covered by the `onebrain-search` crate's `FakeEmbedder` tests; the HTTP tests assert the download-free paths (empty-pending no-op, concurrent search+status, auth, 503-without-engine).

## Consequences

- With the client tracks wired in, mcp + CLI search route through the one daemon engine, so multiple concurrent sessions no longer race redb's single-writer lock.
- `serve` behaviour is unchanged (still opens per-request) — no regression for the foreground command, and every existing server test passes untouched (they build the router with `hold_engine: false`).
- The daemon now cleans up after itself: idle-shutdown releases the redb lock when unused, and `daemon.json` is removed on clean exit so a client never connects to a dead daemon.
- Version skew is handled explicitly: a client that finds a daemon at a different version restarts it before use, so a stale daemon from an older install can't serve an incompatible wire shape.
- The client library (`daemon_client.rs`) currently reads as mostly dead code (allowed at the module level) until the mcp + CLI tracks call it; its core paths are unit-tested so it isn't untested dead code.
- Windows: TCP + `O_EXCL` work everywhere, so the concurrent-start guard and discovery are cross-platform; the detached-spawn / SIGTERM parts of the daemon remain Unix-first as before (unchanged by this PR).
