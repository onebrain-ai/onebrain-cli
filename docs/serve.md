# Local web UI — `onebrain serve`

`onebrain serve` starts a local, token-gated HTTP server that hosts the **OneBrain web UI** — a file explorer, a reading view (markdown, code, PDF, Office docs, images, audio/video, Jupyter notebooks), a native-search-backed search panel, and an agent chat — over a small vault JSON API.

```bash
onebrain serve          # reuse/start this vault's daemon, print its http://127.0.0.1:<PORT>/?token=<TOKEN>
onebrain serve --open   # …and open it in your browser
onebrain serve --port 6789   # force a standalone foreground server on a fixed port (Ctrl-C to stop)
```

The bare `onebrain serve` routes to the [daemon](daemon.md) for the current vault (starting it if needed) and prints THAT daemon's URL — its port is **ephemeral** (per-vault, read from the daemon's slot), not a fixed 6789. Only an explicit `--port`/`--dir` (or `ONEBRAIN_BIND` / `ONEBRAIN_NO_DAEMON=1`) runs a standalone foreground server, which defaults to port **6789**.

> **Breaking (v3.4.8, #205):** the `--host` flag was removed — every listener binds `127.0.0.1`, exactly like the daemon. Remote access is tunnel-only; see [Remote access in daemon.md](daemon.md#remote-access) and the container note below.

The web UI is **embedded in the binary** — a release `onebrain` ships the latest build and `serve` mounts it at `/`, so there's nothing extra to install. Pass `--dir <dist>` only to override the bundle (web UI development against a live daemon).

- **Token-gated** — every request (and the SPA shell itself) needs the per-session token printed in the URL, sent as the `X-OneBrain-Token` header, a `?token=` query param, or an `HttpOnly` cookie.
- **Loopback only** (`127.0.0.1`; the daemon's port is ephemeral, a standalone `serve` defaults to `6789`). There is no bind-address flag; for remote access put an encrypted tunnel in front — see [Remote access in daemon.md](daemon.md#remote-access).
- **Hardened surface** — confined to the vault (tooling dirs like `.git`/`.claude` are refused), script-carrying files forced to download, a strict CSP, and the agent subprocess never inherits the daemon token. See [Security & trust model](install.md#security--trust-model).

**Daemon-aware (v3.4.8; reuse-or-start since v3.4.12; per-vault since v3.4.13):** `serve` **reuses** the [daemon](daemon.md) already serving the current vault — and, when none is running, **starts** one (restarting a stale or version-mismatched one) — then prints THAT daemon's token-bearing URL and exits. Since v3.4.13 (#230) each vault's daemon binds its OWN ephemeral port, so `serve` reads the port from the daemon's slot (it does **not** assume 6789); it never binds a second listener next to the same vault's daemon (both would want that vault's engine lock), while two DIFFERENT vaults' daemons coexisting on their own ephemeral ports is fine. Because the started daemon holds the engine + token cache, the Token-Gain dashboard is populated. That makes `onebrain serve --open` the one command that always lands you in the web UI, whether or not a daemon is up, with no token knowledge required. Passing `--port` or `--dir` (or setting `ONEBRAIN_BIND`) always means a standalone foreground server on the default port 6789 (you asked for a specific listener), and `ONEBRAIN_NO_DAEMON=1` disables daemon reuse/start. Like `onebrain mcp`, `serve` is an active daemon-lifecycle owner for ITS vault's slot (it may start/restart that vault's daemon); the passive CLI search verbs never do.

## Containers / self-host — `ONEBRAIN_BIND`

Inside a container, `127.0.0.1` is unreachable from the host, so loopback-only would make `serve` useless there. The **only** bind escape hatch is the `ONEBRAIN_BIND` env var:

```bash
ONEBRAIN_BIND=0.0.0.0 onebrain serve   # container-internal bind, behind Docker's published-port NAT
```

- Parsed as a plain IP address; an invalid value is a **hard error** (never a silent loopback fallback).
- A non-loopback value prints the loud plaintext-HTTP warning — the token and vault content travel unencrypted, so keep the exposure inside the container network / behind a TLS proxy, exactly as with any tunnel-first deployment.
- Setting it counts as asking for a specific listener, so `serve` won't reroute to a running daemon.

> **`serve` vs the daemon.** `serve` is a foreground, per-session server that opens the search engine per request. The persistent [`onebrain daemon`](daemon.md) runs the same surface but detached and **holds the search engine for its lifetime** (the single redb owner for that vault's mcp + CLI search), adding `/api/internal/*` reindex/status endpoints and a per-vault `~/.onebrain/run/daemon-<hash>.json` discovery file (one slot per vault; each on its own ephemeral port). Its `daemon status` prints a full dashboard for every running daemon, including the clickable webui URL. See [ADR 0023](decisions/0023-warm-daemon-mcp-search.md) and [ADR 0033](decisions/0033-per-vault-daemon-slots.md).
