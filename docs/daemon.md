# Warm daemon — `onebrain daemon`

The **daemon** is a long-lived, detached process that runs the same local HTTP surface as [`onebrain serve`](serve.md) but persists across sessions and **owns the native-search engine** for its whole lifetime.

```bash
onebrain daemon start                # spawn the detached daemon for the cwd vault (no-op if one is already running)
onebrain daemon start --vault <path> # …for a specific vault
onebrain daemon status               # dashboard for EVERY running daemon (process / bind / webui URL / engine / models)
onebrain daemon stop                 # SIGTERM the cwd vault's daemon, clean up its slot files
onebrain daemon stop --vault <path>  # …a specific vault's daemon
onebrain daemon stop --all           # …every daemon on the machine
```

> **Per-vault slots (v3.4.13, #230).** Each vault gets its OWN daemon, keyed on a hash of its canonical path — so multiple vaults' warm daemons coexist on one machine, each on its own ephemeral port + its own slot files (`daemon-<hash>.{json,pid,lock,log}`). Two vaults' concurrent sessions no longer thrash (a `discover` on one vault never stops the other vault's daemon). A vault-less daemon uses the `daemon-none.*` sentinel slot. See [ADR 0033](decisions/0033-per-vault-daemon-slots.md).

## Why it exists

The search engine's metadata store (`engine.redb`) is **single-process**: only one process may open a collection's engine at a time. With multiple concurrent sessions (webui, CLI, agent-teams) each opening their own engine, they collide with redb's single-writer limit. The daemon opens the engine **once at boot** and holds it, so the other surfaces talk to it over HTTP instead of each opening their own. See [ADR 0023](decisions/0023-warm-daemon-mcp-search.md).

> **Scope (v3.4.6, per-vault since v3.4.13):** the daemon owns the **search** engine and exposes reindex + status + get endpoints; the reusable client library ships alongside it. Both the **MCP server** (`onebrain mcp`) and the **CLI `search` verbs** now *use* the daemon:
> - **MCP (Track 2b) — ACTIVE lifecycle owner.** Its `status`/`query` tools route through `daemon_client::ensure_running` (`GET /api/internal/status` + `GET /api/vault/search`), which STARTS THIS vault's slot daemon if none is up and RESTARTS it on a same-slot version skew. Since v3.4.13 it only ever touches ITS OWN vault's slot — a daemon for a DIFFERENT vault lives in a different slot and is left running. Falls back to a direct single-process engine open only when the daemon can't start.
> - **CLI (Track 2c) — PASSIVE reader.** `onebrain search …` routes through `daemon_client::discover_matching` — it uses a daemon only when one is ALREADY running in this vault's slot, and **never starts or restarts** one (a `search` command must not disrupt a live MCP session on another vault). On any mismatch (no daemon / wrong version / dead) it opens the engine directly. See [CLI search verbs route through the daemon](#cli-search-verbs-route-through-the-daemon-track-2c) below.
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
| `GET  /api/health`           | `{ ok, engine_held, dist_dir }` — **engine-independent** liveness: 200 whenever the process is up, whether or not an engine is held. `dist_dir` (v3.4.8) is the served web UI source: `null` = the UI embedded in the binary, a string = the `$ONEBRAIN_DIST` override path. (The vault a client matches against is read from the slot's `daemon-<hash>.json` `vault` field, not surfaced here) |

- `mode: "pending"` embeds exactly the docs `Engine::pending_vector_paths` reports as drifted (same as `onebrain search reindex --pending-only`). `mode: "paths"` reindexes the caller-supplied vault-relative doc paths. `mode: "lex"` is a keyword-only whole-vault pass with **no embedding** (mirrors `--lex-only`; the PostToolUse hook uses it in-session without a per-write model load). `mode: "all"` is a full whole-vault reindex + embed (a bare `search reindex`).
- **Path safety (defense-in-depth):** every `paths` entry is confined to the vault at the HTTP layer before it reaches the engine — absolute paths, `..` traversal, symlink-escapes, and tooling dirs (`.git`/`.claude`/…) are rejected with `400`, so a reindex can't be steered into arbitrary files (`../../etc/passwd`). The **engine** (`Engine::reindex_paths`) ALSO confines each path (rejects absolute/`..`/symlink-escape as "outside the vault", skips it, indexes nothing) — so the DIRECT reindex path that bypasses the daemon (`ONEBRAIN_NO_DAEMON=1`, daemon-unavailable fallback, plain `onebrain search reindex <paths>`) is protected by the same rule, not just the HTTP boundary (#175).
- The `/internal/*` routes require the daemon to actually **hold** an engine; a `serve` process (which doesn't) returns `503` rather than opening one per-request. `/api/health` does NOT — it's a pure liveness check. Reindex writes serialise on the engine mutex, so a concurrent `/api/vault/search` interleaves between batches.
- The webui `GET /api/vault/search` uses the **held** engine when the daemon is running (lex still uses the standalone tantivy index, which never touches redb).

## The daemon serves the web UI — always

The daemon runs the **same static surface as [`serve`](serve.md)**: with `$ONEBRAIN_DIST` unset (the default) it serves the **web UI embedded in the binary**; `$ONEBRAIN_DIST` is an override for webui development / the plugin launcher pinning a dist. There is **no API-only mode** — a release binary's daemon always answers `GET /` with the full UI (a from-source build without bundled assets falls back to a token-bearing placeholder page). Every route, the UI shell included, is behind the per-process token.

Two ways to open it without ever reading a slot json by hand:

- **`onebrain daemon status`** prints the clickable `http://127.0.0.1:PORT/?token=TOKEN` as part of its dashboard (below).
- **`onebrain serve --open`** reuses a daemon already serving the current vault — or starts one when none is running (v3.4.12) — and opens the browser at that daemon's URL instead of binding a second listener — see [serve.md](serve.md).

## `daemon status` — a real dashboard

`daemon status` reports much more than running/pid (v3.4.8, #197). It **enumerates every `daemon-*` slot** (v3.4.13, #230) and, for each running daemon, reads its slot json and best-effort-probes `GET /api/health` + `GET /api/internal/status`, then renders one grouped block per daemon:

```text
2 daemons running               ← count header, only when > 1

🟢  Process        pid · version · started · idle TTL
🔌  Bind           port (this daemon's ephemeral port) · bound vault · web UI source (embedded vs $ONEBRAIN_DIST)
🌐  Webui          http://127.0.0.1:PORT/?token=…   ← click/copy to open
🧠  Engine         held · docs · pending · last indexed
🎯  Models         embed model · reranker (readiness)
```

- **Enumerates all vaults:** with several vaults' daemons up, each gets its own block (its own ephemeral port). No daemon running → `daemon not running`.
- **Legacy daemon visible (v3.4.13):** a live pre-v3.4.13 machine-wide daemon (`daemon.json`, no `-<hash>`) is surfaced too, so it isn't invisibly holding a vault's lock during the upgrade window (see [Upgrading from a pre-v3.4.13 daemon](#upgrading-from-a-pre-v3413-daemon)).
- **Degrades, never fails:** every dashboard field is optional — a failed probe (or an engine-less daemon, whose `/api/internal/status` 503s) just omits its section; `status` still exits 0. `--json` returns a **list**: `{ "daemons": [ {running, pid, port, vault, …}, … ] }` (was a single `{running, pid}` object before v3.4.13 — the single→list shape change).
- **Read-only:** the status probes never start, stop, or restart a daemon.
- **Token note:** the webui URL carries the token. Printing it to your own terminal is fine — it already sits user-readable in the slot json (0600) — and it is never written to the daemon log/tracing.

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
- **Passive: route-only, never start or restart.** The CLI uses `daemon_client::discover_matching`, NOT the MCP path's active `discover()`/`ensure_running()`. A plain `onebrain search …` never spawns a daemon and never stops/restarts one — it routes only to a daemon that is ALREADY up in this vault's slot. This is the load-bearing difference from the MCP path, which owns its slot daemon's lifecycle and restarts it on a same-slot version skew. The CLI must never disrupt a daemon serving another vault.
- **Vault-match by slot.** Each vault has its OWN slot (`daemon-<hash>.json`), so `discover_matching` resolves THIS vault's slot directly — a daemon started for vault A lives in A's slot and is simply never read by a `--vault B` request (which reads B's slot). It routes only when B's slot holds a daemon at OUR `version_decision` AND `record_is_our_vault` confirms the slot record's vault is ours (a defensive check against a hash collision / stale record) AND the daemon passes the engine-independent `/api/health` liveness probe. On any mismatch it declines (direct open) and **leaves the slot record untouched** — no stop, no remove.
- **`search search` (lex)** is NOT daemon-routed: it uses the standalone tantivy index (no redb, no contention). **`search vsearch` (vector-only)** IS daemon-routed since v3.4.12 (#258 Gap 3): it routes through the daemon's `GET /api/vault/search?mode=vec` — passively, via `route_to_daemon` like the other search verbs — so it returns results while a session holds the engine instead of an `E_ENGINE_BUSY`.
- **A routed request stays honest when the daemon holds no engine.** During the upgrade-transition window a *pre-3.4.6* `onebrain mcp` can still own the redb lock while the new warm daemon runs; the daemon is then engine-less and its `/api/internal/*` + `/api/vault/search` routes return `503`. The CLI classifies that 503 as `E_ENGINE_BUSY` (exit 77) for `query`/`get`/`reindex` and `busy: true` for `status` — identical to the direct-open fallback — so a routed verb never leaks an internal error mid-transition. (This is unambiguous because the CLI only routes to a *vault-matched* daemon; a vault-less daemon's "no vault bound" 503 is never reached — see [ADR 0023](decisions/0023-warm-daemon-mcp-search.md) Track 3.)
- **Kill switch.** Set `ONEBRAIN_NO_DAEMON=1` to disable all CLI daemon routing (every verb opens the engine directly, the pre-daemon behaviour).

Every route — including `/api/health` — is behind the **same token auth** as the rest of the surface; no separate credential.

## Discovery — per-vault slots `~/.onebrain/run/daemon-<hash>.json`

Each vault's daemon has its OWN slot, keyed on `<hash>` = `short_path_hash(canonical vault path)` — the SAME primitive that keys per-vault search-collection cache dirs, so a vault maps to one stable slot. A vault-less daemon uses the `daemon-none.*` sentinel slot. After it binds, the daemon writes its slot's discovery json so clients can find it:

```json
// ~/.onebrain/run/daemon-8a1a05.json
{ "port": 53153, "token": "…", "pid": 12345, "version": "3.4.13", "vault": "/abs/canonical/vault" }
```

- `port` is the **actual** bound port. Each per-vault daemon binds an **EPHEMERAL** port (OS-assigned `:0`) — two daemons can't share a fixed port — and publishes the real value here. **Nothing assumes 6789**: discovery/serve/mcp/status all read the port from the slot. (`$ONEBRAIN_DAEMON_PORT` still overrides for a single-daemon convenience / tests.)
- `vault` is the **canonical path** the daemon bound at boot (`null` for the vault-less sentinel slot). A client resolves ITS vault's slot with `resolve_slot(expected_vault)` and reads only that `daemon-<hash>.json` — **there is no cross-vault comparison and no stealing** of another vault's daemon. The stored `vault` is still checked defensively (`record_is_our_vault`) to guard a hash collision or a stale record from a moved vault. A caller whose vault won't canonicalize resolves to `SlotResolve::Unresolvable` and conservatively routes direct / refuses to start (never adopts an unverifiable daemon).
- The file is `0600` (it holds the token) and written atomically (temp-then-rename).
- It is **removed on clean shutdown** (SIGTERM / `daemon stop`). A **hard kill** (SIGKILL/crash) leaves it behind — there's no crash-time cleanup — so `discover()` treats every record as untrusted: it liveness-probes and **reclaims a stale record lazily** on the next lookup, so a client never connects to a dead daemon.

Runtime files for each slot live under `~/.onebrain/run/`: `daemon-<hash>.{pid,log,json}` and the transient `daemon-<hash>.lock` (below). `daemon status` enumerates every `daemon-*` slot; `daemon stop --all` clears them all.

## Concurrent-start guard

Two `daemon start` calls racing in parallel used to be able to both spawn (a TOCTOU race). `daemon start` now takes a **per-slot exclusive `O_EXCL`-create lock** (`daemon-<hash>.lock`) around the check-then-spawn window: exactly one wins and binds; the others report "already running". Because the lock is **per-slot**, two DIFFERENT vaults starting concurrently never serialize (they take different locks); same-vault concurrent starts still mutually exclude. The lock is **self-describing** — it records the creating `daemon start` PID, and a contender reclaims it only if that PID is provably dead (a crashed starter); a live creator (or an unreadable PID) is respected. The winner **holds the lock until the daemon has published its slot json** (fully bound), so serialized same-vault racers always observe a running daemon and back off. Result: N simultaneous starts for one vault → exactly one daemon. Cross-platform (`create_new` = `O_EXCL` / `CREATE_NEW`), no `flock` dependency.

> A loser (`Contended`) reports the winner's real PID: it briefly polls the slot's PID file before falling back to `0`, so a race lost by a few ms never surfaces a misleading `pid 0`.

**Wedged-lock recovery.** A `daemon start` that is SIGKILL'd inside the check-then-spawn window can leave `daemon-<hash>.lock` behind; if the OS later recycles that PID onto an unrelated live process, the lock never looks stale and `daemon start` would report "already running" forever. **`onebrain daemon stop` unlinks the slot's lock** (along with its pid/json), so `stop` (or `stop --all`) is the CLI recovery — no manual `rm` needed.

## Auto-start + client library

The client library (`commands/daemon_client.rs`) is what the CLI/MCP tracks call to reach the daemon:

- `resolve_slot(expected_vault)` — the ONE shared resolver (used by both the daemon and clients): `None` → the vault-less sentinel slot; a canonicalizable path → that vault's `daemon-<hash>` slot; an uncanonicalizable path → `SlotResolve::Unresolvable` (no trustworthy slot). Its `SlotPaths` gives the `{json,pid,lock,log}` filenames.
- `discover(expected_vault)` — **ACTIVE (MCP path).** Resolve THIS vault's slot, read its json, liveness-probe via **`GET /api/health`** (the engine-independent route — probing `/internal/status` would wrongly read a live-but-engine-less daemon as dead and delete its record), return a handle; a stale/dead record is cleaned up and yields `None`. On a same-slot **version** skew it stops THAT slot's daemon (in-process) and yields `None` so `ensure_running` starts a fresh one. It **never stops another vault's daemon** — they live in different slots.
- `ensure_running(expected_vault)` — `discover()`, else spawn `daemon start` **`--vault <path>`** detached and poll this vault's slot for readiness with a bounded timeout. The vault is passed to the spawned daemon as an **explicit CLI argument** (`daemon start --vault …` → threaded to the detached `daemon __run --vault …` child → `resolve_daemon_vault`), NOT by mutating the caller's `$ONEBRAIN_VAULT` (a `std::env::set_var`, which is unsound under concurrent reads and deprecated since Rust 1.81). The `--vault` arg takes precedence over `$ONEBRAIN_VAULT`, which remains a back-compat fallback. The start race is handled implicitly: if another starter for THIS slot won, its json appears and the client connects to it. On timeout the error **classifies the last state** (no slot json / version-skew / alive-but-no-response) and points at the slot's `daemon-<hash>.log`.
- `discover_matching(expected_vault)` — **PASSIVE (CLI-search path).** Same slot read + `/api/health` liveness check, but returns a handle ONLY for a live daemon in this vault's slot at OUR version; on ANY mismatch it declines (`None` → direct open) **without stopping, restarting, or removing** the record. A dead SAME-vault record is reclaimed like `discover`.
- **Uncanonicalizable expected vault (safety hardening).** `resolve_slot` returns `SlotResolve::Unresolvable` when the caller HAS a vault but `canonical_vault_id` returns `None` (the dir vanished mid-op), rather than collapsing to "any vault". Discovery then yields `None` (route direct) and `ensure_running` bails with a clear message — the client never adopts or spawns a daemon it can't key to a verified vault. (This replaces the pre-slot `VaultExpectation`/`vault_decision` cross-vault machinery, which the per-vault-slot model made unnecessary.)
- `DaemonHandle::search` / `reindex` / `status` / `get` — typed HTTP calls carrying the token. Each **retries once via `ensure_running()`** (reconnecting to the handle's OWN bound vault) on a transport error (a daemon that vanished mid-call), while an HTTP status error propagates unchanged; `get` additionally maps a `404` to `Ok(None)` (doc not indexed) so the CLI renders its "not indexed yet" hint.
- **Version skew (ACTIVE path):** when this vault's slot json carries a `version` different from the caller's, `discover`/`ensure_running` restart THAT slot's daemon (stop + start) before use, so a daemon from an older install can't serve the wrong wire shape. A failed stop is surfaced as a warning (the old daemon may still hold the engine lock), not swallowed. The PASSIVE `discover_matching` never restarts — it routes direct instead.

## Remote access

The daemon deliberately binds **`127.0.0.1` only** — there is no bind-address override. To use the web UI from another machine, put an **encrypted tunnel** in front of it; never expose the port directly (the token and all vault content would travel as plain HTTP). Each per-vault daemon binds a DIFFERENT ephemeral port — run `onebrain daemon status` to read the one you want to tunnel (`PORT` below).

Three recommended paths, in increasing order of setup:

1. **SSH port forward** — simplest, good for occasional use from another machine you already SSH into:

   ```bash
   ssh -L PORT:127.0.0.1:PORT user@host    # PORT from `onebrain daemon status`
   ```

   Then open `http://127.0.0.1:PORT/?token=TOKEN` in the *local* machine's browser — `onebrain daemon status` on the daemon's machine prints the full clickable URL.

2. **Tailscale Serve** — recommended for regular or mobile use; HTTPS with tailnet-only identity, no ports exposed to the internet:

   ```bash
   tailscale serve https / http://127.0.0.1:PORT
   ```

3. **Cloudflare Tunnel + Access** — for reaching it over the public internet behind SSO.

Notes:

- Set `ONEBRAIN_TOKEN` (**≥ 32 chars**, e.g. `openssl rand -hex 16`) in the daemon's environment for a **stable, bookmarkable** token across daemon restarts — otherwise every restart mints a fresh token and saved URLs go stale. (The port can still change per restart unless you also pin `$ONEBRAIN_DAEMON_PORT`.) The value must match the charset `[A-Za-z0-9_-]`; a too-short or invalid value is a hard error that prevents the daemon from starting. Unset the variable to get a generated token instead.
- Do **not** force a standalone `serve` alongside the same vault's running daemon — they'd both want that vault's engine lock (a plain `serve` reuses or starts the daemon and routes to it instead of binding; forcing a second listener needs an explicit `--port`). Two DIFFERENT vaults' daemons coexisting on their own ephemeral ports is fine. The `serve --host` flag no longer exists (#205): the only non-loopback bind is the container-scoped `ONEBRAIN_BIND` env var, which warns loudly that it speaks plaintext HTTP (see [serve.md](serve.md#containers--self-host--onebrain_bind)).

## Upgrading from a pre-v3.4.13 daemon

Before v3.4.13 a single machine-wide `daemon.json` daemon held the vault's redb lock. New v3.4.13 sessions read **slot** files (`daemon-<hash>.json`) and never touch that legacy record, so **an old daemon left running keeps holding the lock**: a new slot daemon for the same vault can't open the engine and serves engine-less (503) until the old one idles out (up to 30 min), and CLI reads fall back to a direct open that also hits the lock (`E_ENGINE_BUSY`). This is a transient upgrade-window state, not a crash.

- **Make it visible:** `onebrain daemon status` and `onebrain doctor` both surface a live legacy daemon (with the retire hint) rather than reporting "not running".
- **Retire it now:** `onebrain daemon stop --all` stops every per-vault slot **and** the legacy daemon, ending the window immediately. A dead legacy record is inert and ignored (no action needed).

## Automatic retire on same-family upgrade (v3.4.15, #291)

A same-family binary upgrade (e.g. `3.4.13 → 3.4.14`) has the same hazard in a subtler form: a still-running warm daemon of the OLD version is re-adopted by the next `daemon start`/`serve`/`mcp` (discovery adopts a same-vault daemon regardless of version), so it keeps serving the **old wire shape** — e.g. a superseded `token gain` route leaves the WebUI dashboard dark — until it idles out (~30 min). Unlike the pre-v3.4.13 legacy case there's no lock collision, just stale routes, so it previously slipped the manual "stop --all after upgrading" net. v3.4.15 automates the refresh:

- **`onebrain update`** — after a real self-update passes the validate gate, retires **all** warm daemons machine-wide (they respawn at the new version on next use). Reported as `↻ retired {n} warm daemon(s)`.
- **`onebrain plugin update`** — retires this vault's warm daemon **only when its version is skewed** from the CLI's (the `brew upgrade` + `plugin update` flow); a matching-version daemon is left warm (no cold-start penalty).
- **`onebrain doctor`** — the safety net when a user `brew upgrade`s directly and runs neither: it **warns** on a live version-skewed daemon (naming both versions) with the `onebrain daemon stop --all` hint. Diagnostic only — doctor never stops a daemon.

So the manual `daemon stop --all` after an in-place upgrade is now a fallback, not a required step — `update`/`plugin update` handle the common paths and `doctor` flags whatever slips through.

## Lifecycle

- **Idle-shutdown TTL** — after `$ONEBRAIN_DAEMON_IDLE_SECS` (default **30 min**) with no authenticated request, the daemon exits, dropping the engine and releasing the redb lock. Set `0` to disable (run forever — e.g. a pinned always-on daemon).
- **Clean SIGTERM** — `daemon stop` (or any SIGTERM) drains in-flight requests, drops the engine, and clears the slot's runtime files (`daemon-<hash>.{pid,json,lock}`).
- **Port** — each per-vault daemon binds `127.0.0.1` on an **ephemeral** OS-assigned port (published in its slot json); `$ONEBRAIN_DAEMON_PORT` overrides it with a fixed value (a single-daemon convenience / for tests).

## Differences from `serve`

| | `onebrain serve` | `onebrain daemon` |
|--|------------------|-------------------|
| lifetime | foreground, until Ctrl-C | detached, persists across sessions |
| shutdown | Ctrl-C (SIGINT) | SIGTERM / idle-timeout |
| engine | opened **per request** | opened **once**, held for the process |
| internal routes | 503 (no engine held) | live |
| discovery file | none | per-vault `~/.onebrain/run/daemon-<hash>.json` |

See [ADR 0023](decisions/0023-warm-daemon-mcp-search.md) for the warm-daemon rationale (why the transport reuses the existing localhost HTTP + token auth rather than a bespoke unix socket) and [ADR 0033](decisions/0033-per-vault-daemon-slots.md) for the per-vault-slot model.
