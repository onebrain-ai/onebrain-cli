# Local web UI — `onebrain serve`

`onebrain serve` starts a local, token-gated HTTP server that hosts the **OneBrain web UI** — a file explorer, a reading view (markdown, code, PDF, Office docs, images, audio/video, Jupyter notebooks), a native-search-backed search panel, and an agent chat — over a small vault JSON API.

```bash
onebrain serve          # → http://127.0.0.1:6789/?token=<TOKEN>   (Ctrl-C to stop)
onebrain serve --open   # …and open it in your browser
```

The web UI is **embedded in the binary** — a release `onebrain` ships the latest build and `serve` mounts it at `/`, so there's nothing extra to install. Pass `--dir <dist>` only to override the bundle (web UI development against a live daemon).

- **Token-gated** — every request (and the SPA shell itself) needs the per-session token printed in the URL, sent as the `X-OneBrain-Token` header, a `?token=` query param, or an `HttpOnly` cookie.
- **Loopback by default** (`127.0.0.1:6789`). `--host 0.0.0.0` self-hosts remotely but serves plain HTTP — put a TLS tunnel/proxy (Cloudflare Tunnel, Tailscale Serve, Caddy) in front; `serve` warns loudly when you bind beyond loopback. For remote access to a running **daemon** (which always binds loopback), see [Remote access in daemon.md](daemon.md#remote-access).
- **Hardened surface** — confined to the vault (tooling dirs like `.git`/`.claude` are refused), script-carrying files forced to download, a strict CSP, and the agent subprocess never inherits the daemon token. See [Security & trust model](install.md#security--trust-model).

**Daemon-aware (v3.4.8):** `serve` and the [daemon](daemon.md) share port 6789 by design (one surface, one port), so if a daemon is already serving the current vault, `serve` doesn't bind a second listener — it prints the daemon's token-bearing URL, honours `--open`, and exits. That makes `onebrain serve --open` the one command that always lands you in the web UI, whether or not a daemon is up, with no token knowledge required. Passing `--port`, `--host`, or `--dir` always means a standalone server (you asked for a specific listener), and `ONEBRAIN_NO_DAEMON=1` disables the daemon detection. `serve` never starts, stops, or restarts a daemon.

> **`serve` vs the daemon.** `serve` is a foreground, per-session server that opens the search engine per request. The persistent [`onebrain daemon`](daemon.md) runs the same surface but detached and **holds the search engine for its lifetime** (the single redb owner for mcp + CLI search), adding `/api/internal/*` reindex/status endpoints and a `~/.onebrain/run/daemon.json` discovery file. Its `daemon status` prints a full dashboard including the clickable webui URL. See [ADR 0023](decisions/0023-warm-daemon-mcp-search.md).
