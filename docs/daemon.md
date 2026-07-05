# Warm daemon — `onebrain daemon`

The **daemon** is a long-lived, detached process that runs the same local HTTP surface as [`onebrain serve`](serve.md) but persists across sessions and **owns the native-search engine** for its whole lifetime.

```bash
onebrain daemon start    # spawn the detached daemon (no-op if one is already running)
onebrain daemon status   # is it running? which PID?
onebrain daemon stop     # SIGTERM it, clean up its runtime files
```

## Why it exists

The search engine's metadata store (`engine.redb`) is **single-process**: only one process may open a collection's engine at a time. With multiple concurrent sessions (webui, CLI, agent-teams) each opening their own engine, they collide with redb's single-writer limit. The daemon opens the engine **once at boot** and holds it, so the other surfaces talk to it over HTTP instead of each opening their own. See [ADR 0023](decisions/0023-warm-daemon-mcp-search.md).

> **Scope (v3.4.6):** the daemon owns the **search** engine and exposes reindex + status endpoints; the reusable client library ships alongside it. Wiring the CLI `search` verbs and the MCP server to *use* the daemon lands in follow-up tracks — until then those surfaces still open the engine directly. Consolidating the remaining surfaces (config/tree/file/chat) behind the daemon is later cleanup.

## Persistent engine + internal endpoints

`daemon __run` (the detached body, spawned by `daemon start`) opens the engine once and holds it in-process. On top of the usual [`serve` surface](serve.md) it adds two token-gated **internal** routes:

| route | body / returns |
|-------|----------------|
| `POST /api/internal/reindex` | `{ "mode": "pending" \| "paths", "paths"?: [..] }` → runs the corresponding engine reindex, returns `{ added, updated, removed, unchanged, failed, doc_count }` |
| `GET  /api/internal/status`  | `{ doc_count, pending_new, pending_changed, pending_removed, pending_total, last_indexed, indexed }` — live status from the held engine |

- `mode: "pending"` embeds exactly the docs `Engine::pending_vector_paths` reports as drifted (same as `onebrain search reindex --pending-only`). `mode: "paths"` reindexes the caller-supplied vault-relative doc paths.
- Both routes require the daemon to actually **hold** an engine; a `serve` process (which doesn't) returns `503` rather than opening one per-request. Reindex writes serialise on the engine mutex, so a concurrent `/api/vault/search` interleaves between batches.
- The webui `GET /api/vault/search` uses the **held** engine when the daemon is running (lex still uses the standalone tantivy index, which never touches redb).

Every internal route is behind the **same token auth** as the rest of the surface — no separate credential.

## Discovery — `~/.onebrain/run/daemon.json`

After it binds, the daemon writes a discovery file so clients can find it:

```json
{ "port": 6789, "token": "…", "pid": 12345, "version": "3.4.6" }
```

- `port` is the **actual** bound port (relevant when the daemon binds `0` for an OS-assigned port).
- The file is `0600` (it holds the token) and written atomically (temp-then-rename).
- It is **removed on clean shutdown**, so a client never connects to a dead daemon.

Runtime files live under `~/.onebrain/run/`: `daemon.pid`, `daemon.log`, `daemon.json`, and the transient `daemon.lock` (below).

## Concurrent-start guard

Two `daemon start` calls racing in parallel used to be able to both spawn (a TOCTOU race). `daemon start` now takes an **exclusive `O_EXCL`-create lock** (`daemon.lock`) around the check-then-spawn window: exactly one wins and binds; the other reads the discovery/PID and reports "already running". A stale lock left by a crashed daemon (no live PID) is reclaimed once. This uses `create_new` (`O_EXCL` / `CREATE_NEW`), so it is **cross-platform** — no `flock` dependency.

## Auto-start + client library

The client library (`commands/daemon_client.rs`) is what the CLI/MCP tracks call to reach the daemon:

- `discover()` — read `daemon.json`, liveness-probe the daemon, return a handle; a stale/dead record is cleaned up and yields `None`.
- `ensure_running()` — `discover()`, else spawn `daemon start` detached and poll for readiness with a bounded timeout. The start race is handled implicitly: if another starter won, its `daemon.json` appears and the client connects to it.
- `DaemonHandle::search` / `reindex` / `status` — typed HTTP calls carrying the token.
- **Version skew:** when `daemon.json.version` differs from the client's own version, the client restarts the daemon (stop + start) before use, so a daemon from an older install can't serve an incompatible wire shape.

## Lifecycle

- **Idle-shutdown TTL** — after `$ONEBRAIN_DAEMON_IDLE_SECS` (default **30 min**) with no authenticated request, the daemon exits, dropping the engine and releasing the redb lock. Set `0` to disable (run forever — e.g. a pinned always-on daemon).
- **Clean SIGTERM** — `daemon stop` (or any SIGTERM) drains in-flight requests, drops the engine, and removes `daemon.pid` + `daemon.json`.
- **Port** — the daemon binds `127.0.0.1` on the shared default `6789`; `$ONEBRAIN_DAEMON_PORT` overrides it (`0` = OS-assigned ephemeral, published via `daemon.json`).

## Differences from `serve`

| | `onebrain serve` | `onebrain daemon` |
|--|------------------|-------------------|
| lifetime | foreground, until Ctrl-C | detached, persists across sessions |
| shutdown | Ctrl-C (SIGINT) | SIGTERM / idle-timeout |
| engine | opened **per request** | opened **once**, held for the process |
| internal routes | 503 (no engine held) | live |
| discovery file | none | `~/.onebrain/run/daemon.json` |

See [ADR 0023](decisions/0023-warm-daemon-mcp-search.md) for the full rationale, including why the transport reuses the existing localhost HTTP + token auth rather than a bespoke unix socket.
