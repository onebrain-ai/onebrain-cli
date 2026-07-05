# Local web UI — `onebrain serve`

`onebrain serve` starts a local, token-gated HTTP server that hosts the **OneBrain web UI** — a file explorer, a reading view (markdown, code, PDF, Office docs, images, audio/video, Jupyter notebooks), a native-search-backed search panel, and an agent chat — over a small vault JSON API.

```bash
onebrain serve          # → http://127.0.0.1:6789/?token=<TOKEN>   (Ctrl-C to stop)
onebrain serve --open   # …and open it in your browser
```

The web UI is **embedded in the binary** — a release `onebrain` ships the latest build and `serve` mounts it at `/`, so there's nothing extra to install. Pass `--dir <dist>` only to override the bundle (web UI development against a live daemon).

- **Token-gated** — every request (and the SPA shell itself) needs the per-session token printed in the URL, sent as the `X-OneBrain-Token` header, a `?token=` query param, or an `HttpOnly` cookie.
- **Loopback by default** (`127.0.0.1:6789`). `--host 0.0.0.0` self-hosts remotely but serves plain HTTP — put a TLS tunnel/proxy (Cloudflare Tunnel, Tailscale Serve, Caddy) in front; `serve` warns loudly when you bind beyond loopback.
- **Hardened surface** — confined to the vault (tooling dirs like `.git`/`.claude` are refused), script-carrying files forced to download, a strict CSP, and the agent subprocess never inherits the daemon token. See [Security & trust model](install.md#security--trust-model).

> **`serve` vs the daemon.** `serve` is a foreground, per-session server that opens the search engine per request. The persistent [`onebrain daemon`](daemon.md) runs the same surface but detached and **holds the search engine for its lifetime** (the single redb owner for mcp + CLI search), adding `/api/internal/*` reindex/status endpoints and a `~/.onebrain/run/daemon.json` discovery file. See [ADR 0023](decisions/0023-warm-daemon-mcp-search.md).
