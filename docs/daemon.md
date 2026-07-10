# Warm daemon — `onebrain daemon`

The **daemon** is a long-lived, detached process that runs the same local HTTP surface as [`onebrain serve`](serve.md) but persists across sessions and **owns the native-search engine** for its whole lifetime.

```bash
onebrain daemon start    # spawn the detached daemon (no-op if one is already running)
onebrain daemon status   # full dashboard: process / bind / webui URL / engine / models
onebrain daemon stop     # SIGTERM it, clean up its runtime files
```

## Why it exists

The search engine's metadata store (`engine.redb`) is **single-process**: only one process may open a collection's engine at a time. With multiple concurrent sessions (webui, CLI, agent-teams) each opening their own engine, they collide with redb's single-writer limit. The daemon opens the engine **once at boot** and holds it, so the other surfaces talk to it over HTTP instead of each opening their own. See [ADR 0023](decisions/0023-warm-daemon-mcp-search.md).

> **Scope (v3.4.6):** the daemon owns the **search** engine and exposes reindex + status + get endpoints; the reusable client library ships alongside it. Both the **MCP server** (`onebrain mcp`) and the **CLI `search` verbs** now *use* the daemon:
> - **MCP (Track 2b) — ACTIVE lifecycle owner.** Its `status`/`query` tools route through `daemon_client::ensure_running` (`GET /api/internal/status` + `GET /api/vault/search`), which STARTS a daemon if none is up and RESTARTS it on a version/vault mismatch (one daemon per machine — the MCP session owns switching it). Falls back to a direct single-process engine open only when the daemon can't start.
> - **CLI (Track 2c) — PASSIVE reader.** `onebrain search …` routes through `daemon_client::discover_matching` — it uses a daemon only when one is ALREADY running and serves this exact vault, and **never starts or restarts** one (a `search` command must not disrupt a live MCP session on another vault). On any mismatch (no daemon / wrong vault / wrong version / dead) it opens the engine directly. See [CLI search verbs route through the daemon](#cli-search-verbs-route-through-the-daemon-track-2c) below.
>
> Consolidating the remaining surfaces (config/tree/file/chat) behind the daemon is later cleanup.
>
> **Daemon-routed `query` deltas (MCP tool):** the daemon-backed `query` path differs slightly from the direct-engine path — results are **doc-level** (dedup/fusion keys on `path`, so at most one hit per document, vs multiple chunks of one doc directly); a `vec` sub-query is served as **`hybrid`** (lex mixed in) on the wire; and the fan-out over-fetches the same depth as the direct path (`fetch_k` ≈30 per sub-query, v3.4.7+) so RRF fusion depth matches — no 20-vs-30 asymmetry. `hyde` ≡ `vec` on **both** paths already (both embed the passage text), so daemon routing adds no new hyde loss. `status` is unaffected — its response shape is identical on both paths.

## Persistent engine + internal endpoints

`daemon __run` (the detached body, spawned by `daemon start`) opens the engine once and holds it in-process. On top of the usual [`serve` surface](serve.md) it adds token-gated **internal** routes:

| route | body / returns |
|-------|----------------|
| `POST /api/internal/reindex` | `{ "mode": "pending" \| "paths" \| "lex" \| "all", "paths"?: [..] }` → runs the corresponding engine reindex, returns `{ added, updated, removed, unchanged, failed, doc_count, embed_model }` |
| `GET  /api/internal/status`  | `{ doc_count, pending_new, pending_changed, pending_removed, pending_total, last_indexed, indexed, embed_model, reranker_model, reranker_ready, reranker_downloaded, reranker_disk_bytes }` — live status from the held engine (model fields v3.4.7/v3.4.8) |
| `GET  /api/internal/get`     | `?path=<vault-relative doc>` → `{ doc_path, content }`, or `404` when the doc isn't indexed — the held engine's `Engine::get` (a redb read) |
| `POST /api/internal/rerank`  | `{ "query": <text>, "paths": [<vault-relative doc>..], "top_k": <n> }` → doc-level Tier-2 rerank of the candidate `paths` against `query` via the held engine (`Engine::rerank_paths`), returns `{ "hits": [{ path, score, title, snippet, rerank_score? }] }`. Lets the MCP `query` tool rerank its fused pool through the warm daemon instead of opening a second engine (model stays single-owner). `503` when no engine is held; `400` on malformed JSON |
| `GET  /api/health`           | `{ ok, engine_held, dist_dir }` — **engine-independent** liveness: 200 whenever the process is up, whether or not an engine is held. `dist_dir` (v3.4.8) is the served web UI source: `null` = the UI embedded in the binary, a string = the `$ONEBRAIN_DIST` override path. (The vault a client matches against is read from `daemon.json`'s `vault`, not surfaced here) |

- `mode: "pending"` embeds exactly the docs `Engine::pending_vector_paths` reports as drifted (same as `onebrain search reindex --pending-only`). `mode: "paths"` reindexes the caller-supplied vault-relative doc paths. `mode: "lex"` is a keyword-only whole-vault pass with **no embedding** (mirrors `--lex-only`; the PostToolUse hook uses it in-session without a per-write model load). `mode: "all"` is a full whole-vault reindex + embed (a bare `search reindex`).
- **Path safety (defense-in-depth):** every `paths` entry is confined to the vault at the HTTP layer before it reaches the engine — absolute paths, `..` traversal, symlink-escapes, and tooling dirs (`.git`/`.claude`/…) are rejected with `400`, so a reindex can't be steered into arbitrary files (`../../etc/passwd`). The **engine** (`Engine::reindex_paths`) ALSO confines each path (rejects absolute/`..`/symlink-escape as "outside the vault", skips it, indexes nothing) — so the DIRECT reindex path that bypasses the daemon (`ONEBRAIN_NO_DAEMON=1`, daemon-unavailable fallback, plain `onebrain search reindex <paths>`) is protected by the same rule, not just the HTTP boundary (#175).
- The `/internal/*` routes require the daemon to actually **hold** an engine; a `serve` process (which doesn't) returns `503` rather than opening one per-request. `/api/health` does NOT — it's a pure liveness check. Reindex writes serialise on the engine mutex, so a concurrent `/api/vault/search` interleaves between batches.
- The webui `GET /api/vault/search` uses the **held** engine when the daemon is running (lex still uses the standalone tantivy index, which never touches redb).

## The daemon serves the web UI — always

The daemon runs the **same static surface as [`serve`](serve.md)**: with `$ONEBRAIN_DIST` unset (the default) it serves the **web UI embedded in the binary**; `$ONEBRAIN_DIST` is an override for webui development / the plugin launcher pinning a dist. There is **no API-only mode** — a release binary's daemon always answers `GET /` with the full UI (a from-source build without bundled assets falls back to a token-bearing placeholder page). Every route, the UI shell included, is behind the per-process token.

Two ways to open it without ever reading `daemon.json` by hand:

- **`onebrain daemon status`** prints the clickable `http://127.0.0.1:PORT/?token=TOKEN` as part of its dashboard (below).
- **`onebrain serve --open`** detects a daemon already serving the current vault and opens the browser at the daemon's URL instead of binding a second listener — see [serve.md](serve.md).

## `daemon status` — a real dashboard

`daemon status` reports much more than running/pid (v3.4.8, #197). It reads `daemon.json` and best-effort-probes `GET /api/health` + `GET /api/internal/status`, then renders grouped sections:

```text
🟢  Process        pid · version · started · idle TTL
🔌  Bind           port · bound vault · web UI source (embedded vs $ONEBRAIN_DIST)
🌐  Webui          http://127.0.0.1:6789/?token=…   ← click/copy to open
🧠  Engine         held · docs · pending · last indexed
🎯  Models         embed model · reranker (readiness)
```

- **Degrades, never fails:** every dashboard field is optional — a failed probe (or an engine-less daemon, whose `/api/internal/status` 503s) just omits its section; `status` still exits 0. `--json` carries the same fields (`skip_serializing_if`, so the not-running shape stays the minimal `{running, pid}`).
- **Read-only:** the status probes never start, stop, or restart a daemon.
- **Token note:** the webui URL carries the token. Printing it to your own terminal is fine — it already sits user-readable in `daemon.json` (0600) — and it is never written to `daemon.log`/tracing.

## CLI search verbs route through the daemon (Track 2c)

When a daemon is **already running and serves the same vault**, the CLI search verbs route their request to it instead of opening a second engine — so the CLI keeps working while an `onebrain mcp` session holds the engine, and auto-reindex hooks land in-session:

| verb | routes to | when no daemon (fallback) |
|------|-----------|---------------------------|
| `search query` (hybrid) | `GET /api/vault/search?mode=hybrid` | direct engine open |
| `search status` | `GET /api/internal/status` | direct open → honest `E_ENGINE_BUSY` if the lock is genuinely held |
| `search get` | `GET /api/internal/get` | direct open → honest `E_ENGINE_BUSY` |
| `search reindex` (full) | `POST /api/internal/reindex {mode:"all"}` | direct open (or `--force`, which never routes — it wipes the index files the daemon has open) |
| `search reindex <paths>` | `POST …{mode:"paths"}` | direct open |
| `search reindex --lex-only` (hook) | `POST …{mode:"lex"}` | local gate → skip on `engine-busy` (exit 0) |
| `search reindex --pending-only` (hook) | `POST …{mode:"pending"}` | local detach/foreground path |

Notes:
- **Passive: route-only, never start or restart.** The CLI uses `daemon_client::discover_matching`, NOT the MCP path's active `discover()`/`ensure_running()`. A plain `onebrain search …` never spawns a daemon and never stops/restarts one — it routes only to a daemon that is ALREADY up (the contention an MCP session creates). This is the load-bearing difference from the MCP path, which owns the daemon's lifecycle and restarts it on a version/vault mismatch. The CLI must never disrupt a daemon serving another vault.
- **Vault-match guard.** The machine runs one daemon on a fixed port, so a daemon started for vault A must never answer a `--vault B` request. `discover_matching` reads the daemon's canonical bound vault from `daemon.json` (`DaemonInfo.vault`, written at bind) and routes only when `vault_decision` says it matches the caller's vault AND `version_decision` matches AND the daemon passes the engine-independent `/api/health` liveness probe. On ANY mismatch it declines (direct open) and **leaves the daemon.json record untouched** — no stop, no remove.
- **`search search` (lex)** and **`search vsearch` (vector-only)** are NOT daemon-routed: lex uses the standalone tantivy index (no redb, no contention), and the daemon exposes no vector-only search mode, so vsearch opens directly (honest `E_ENGINE_BUSY` while a session holds the engine — use `query` for results mid-session).
- **A routed request stays honest when the daemon holds no engine.** During the upgrade-transition window a *pre-3.4.6* `onebrain mcp` can still own the redb lock while the new warm daemon runs; the daemon is then engine-less and its `/api/internal/*` + `/api/vault/search` routes return `503`. The CLI classifies that 503 as `E_ENGINE_BUSY` (exit 77) for `query`/`get`/`reindex` and `busy: true` for `status` — identical to the direct-open fallback — so a routed verb never leaks an internal error mid-transition. (This is unambiguous because the CLI only routes to a *vault-matched* daemon; a vault-less daemon's "no vault bound" 503 is never reached — see [ADR 0023](decisions/0023-warm-daemon-mcp-search.md) Track 3.)
- **Kill switch.** Set `ONEBRAIN_NO_DAEMON=1` to disable all CLI daemon routing (every verb opens the engine directly, the pre-daemon behaviour).

Every route — including `/api/health` — is behind the **same token auth** as the rest of the surface; no separate credential.

## Discovery — `~/.onebrain/run/daemon.json`

After it binds, the daemon writes a discovery file so clients can find it:

```json
{ "port": 6789, "token": "…", "pid": 12345, "version": "3.4.6", "vault": "/abs/canonical/vault" }
```

- `port` is the **actual** bound port (relevant when the daemon binds `0` for an OS-assigned port).
- `vault` is the **canonical path** of the vault the daemon bound at boot (`null` if it bound vault-less). The daemon is a **machine singleton**, so a client that resolved a **different** vault must not silently reuse this engine — `discover()`/`ensure_running()` compare `vault` to the caller's resolved vault and, on mismatch, **restart the daemon for the caller's vault** (same handling as a version skew). This makes the v3.4.6 model explicit: **one vault at a time per machine; switching vaults restarts the daemon** (concurrent per-vault daemons are deferred to v3.8). An old `daemon.json` without the `vault` key deserializes to `null`, so a pre-vault-field daemon is never trusted to be serving the right vault.
- The file is `0600` (it holds the token) and written atomically (temp-then-rename).
- It is **removed on clean shutdown** (SIGTERM / `daemon stop`). A **hard kill** (SIGKILL/crash) leaves it behind — there's no crash-time cleanup — so `discover()` treats every record as untrusted: it liveness-probes and **reclaims a stale record lazily** on the next lookup, so a client never connects to a dead daemon.

Runtime files live under `~/.onebrain/run/`: `daemon.pid`, `daemon.log`, `daemon.json`, and the transient `daemon.lock` (below).

## Concurrent-start guard

Two `daemon start` calls racing in parallel used to be able to both spawn (a TOCTOU race). `daemon start` now takes an **exclusive `O_EXCL`-create lock** (`daemon.lock`) around the check-then-spawn window: exactly one wins and binds; the others report "already running". The lock is **self-describing** — it records the creating `daemon start` PID, and a contender reclaims it only if that PID is provably dead (a crashed starter); a live creator (or an unreadable PID) is respected. The winner **holds the lock until the daemon has published `daemon.json`** (fully bound), so serialized racers always observe a running daemon and back off. Result: N simultaneous starts → exactly one daemon. Cross-platform (`create_new` = `O_EXCL` / `CREATE_NEW`), no `flock` dependency.

**Wedged-lock recovery.** A `daemon start` that is SIGKILL'd inside the check-then-spawn window can leave `daemon.lock` behind; if the OS later recycles that PID onto an unrelated live process, the lock never looks stale and `daemon start` would report "already running" forever. **`onebrain daemon stop` unlinks `daemon.lock`** (along with `daemon.pid`/`daemon.json`), so `stop` is the CLI recovery — no manual `rm` needed.

## Auto-start + client library

The client library (`commands/daemon_client.rs`) is what the CLI/MCP tracks call to reach the daemon:

- `discover(expected_vault)` — **ACTIVE (MCP path).** Read `daemon.json`, liveness-probe via **`GET /api/health`** (the engine-independent route — probing `/internal/status` would wrongly read a live-but-engine-less daemon as dead and delete its record), return a handle; a stale/dead record is cleaned up and yields `None`. On a version OR vault mismatch it **stops the daemon** and yields `None` so `ensure_running` starts a fresh one for `expected_vault`.
- `ensure_running(expected_vault)` — `discover()`, else spawn `daemon start` **`--vault <path>`** detached and poll for readiness with a bounded timeout. The vault is passed to the spawned daemon as an **explicit CLI argument** (`daemon start --vault …` → threaded to the detached `daemon __run --vault …` child → `resolve_daemon_vault`), NOT by mutating the caller's `$ONEBRAIN_VAULT` (a `std::env::set_var`, which is unsound under concurrent reads and deprecated since Rust 1.81). The `--vault` arg takes precedence over `$ONEBRAIN_VAULT`, which remains a back-compat fallback. The start race is handled implicitly: if another starter won, its `daemon.json` appears and the client connects to it. On timeout the error **classifies the last state** (no daemon.json / version-skew / alive-but-no-response) and points at `~/.onebrain/run/daemon.log`.
- `discover_matching(expected_vault)` — **PASSIVE (CLI-search path).** Same read + `/api/health` liveness check, but returns a handle ONLY for a live daemon matching BOTH version and vault; on ANY mismatch it declines (`None` → direct open) **without stopping, restarting, or removing** the record (a wrong-vault daemon may be a live MCP session — the CLI must never disrupt it). A dead SAME-vault record is reclaimed like `discover`.
- **Uncanonicalizable expected vault (safety hardening).** All three functions turn the caller's `expected_vault` into a `VaultExpectation` (`Any` / `Vault(id)` / `Unresolvable`) via `vault_expectation`, rather than the old `expected_vault.and_then(canonical_vault_id)` which flattened "has a vault but it wouldn't canonicalize" into "no expectation". If the caller HAS a vault but `canonical_vault_id` returns `None` (the dir vanished mid-op), the expectation is `Unresolvable` → `vault_decision` treats it as a **mismatch**: the CLI routes direct, and the MCP active path refuses to adopt/spawn a daemon it can't verify. This closes a narrow wrong-vault TOCTOU where a caller whose vault vanished would otherwise reuse whatever daemon was up.
- `DaemonHandle::search` / `reindex` / `status` / `get` — typed HTTP calls carrying the token. Each **retries once via `ensure_running()`** (reconnecting to the handle's OWN bound vault) on a transport error (a daemon that vanished mid-call), while an HTTP status error propagates unchanged; `get` additionally maps a `404` to `Ok(None)` (doc not indexed) so the CLI renders its "not indexed yet" hint.
- **Version / vault skew (ACTIVE path):** when `daemon.json`'s `version` or bound `vault` differs from the caller's, `discover`/`ensure_running` restart the daemon (stop + start) for the caller's vault before use, so a daemon from an older install or a different vault can't serve the wrong engine. A failed stop is surfaced as a warning (the old daemon may still hold the engine lock), not swallowed. The PASSIVE `discover_matching` never restarts — it routes direct instead.

## Remote access

The daemon deliberately binds **`127.0.0.1` only** — there is no bind-address override. To use the web UI from another machine, put an **encrypted tunnel** in front of it; never expose port 6789 directly (the token and all vault content would travel as plain HTTP).

Three recommended paths, in increasing order of setup:

1. **SSH port forward** — simplest, good for occasional use from another machine you already SSH into:

   ```bash
   ssh -L 6789:127.0.0.1:6789 user@host
   ```

   Then open `http://127.0.0.1:6789/?token=TOKEN` in the *local* machine's browser — `onebrain daemon status` on the daemon's machine prints the full clickable URL.

2. **Tailscale Serve** — recommended for regular or mobile use; HTTPS with tailnet-only identity, no ports exposed to the internet:

   ```bash
   tailscale serve https / http://127.0.0.1:6789
   ```

3. **Cloudflare Tunnel + Access** — for reaching it over the public internet behind SSO.

Notes:

- Set `ONEBRAIN_TOKEN` (**≥ 32 chars**, e.g. `openssl rand -hex 16`) in the daemon's environment for a **stable, bookmarkable URL** across daemon restarts — otherwise every restart mints a fresh token and saved URLs go stale. A too-short value is ignored (with a warning) in favour of a random token.
- Do **not** run `serve --host 0.0.0.0` while a daemon is running — they share port 6789 and the engine lock. `serve --host` is the foreground-only remote path for when **no daemon** runs, and it warns loudly that it speaks plaintext HTTP (see [serve.md](serve.md)).

## Lifecycle

- **Idle-shutdown TTL** — after `$ONEBRAIN_DAEMON_IDLE_SECS` (default **30 min**) with no authenticated request, the daemon exits, dropping the engine and releasing the redb lock. Set `0` to disable (run forever — e.g. a pinned always-on daemon).
- **Clean SIGTERM** — `daemon stop` (or any SIGTERM) drains in-flight requests, drops the engine, and clears the runtime files (`daemon.pid`, `daemon.json`, and `daemon.lock`).
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
