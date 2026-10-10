# 0042 — Tunnel and service: Cloudflare token file, KeepAlive LaunchAgents, macOS-only for now

- **Status:** accepted
- **Date:** 2026-10
- **Follows:** [0036](0036-remote-mcp-gateway-separate-process.md), [0039](0039-gateway-approval-keepalive-and-wait-clamp.md), [0040](0040-gateway-pre-tunnel-exposure-hardening.md).
- **PRs:** [#419](https://github.com/onebrain-ai/onebrain-cli/pull/419); tracking issue #412.
- **Design:** "Gateway tunnel + service — Design" (decisions D2, D3).

## Context

The phone use case needs a public hostname that reaches the loopback gateway, and a gateway that is always up. The gateway was a foreground process stopped with Ctrl-C, and the project had no `cloudflared` code. A hand-built setup (a tunnel created with `cloudflared tunnel login` / `create` / `route`, two terminals) is too fragile for something that is supposed to survive logout and reboot.

## Decision

- **Cloudflare tunnel, created in the dashboard.** The operator creates a tunnel and a public hostname (service `http://127.0.0.1:<port>`) in Cloudflare Zero Trust, and pastes the tunnel token. `onebrain gateway tunnel setup` validates the hostname shape and the token shape (without printing it), writes the token to `~/.onebrain/gateway/tunnel.token` with mode **0600**, and sets `public_url` in `gateway.yml` through a comment-preserving writer. It uses no `cert.pem` and no `cloudflared` management commands. `tunnel status` checks the token file, `public_url`, the agent, and an HTTPS probe whose OAuth `issuer` must equal `public_url`, which proves the request reached *this* gateway through the tunnel and the host guard.
- **Token handed to `cloudflared` as a file.** The agent runs `cloudflared tunnel --no-autoupdate --protocol http2 run --token-file <path>`. If the installed `cloudflared` has no `--token-file`, the token goes into the agent's environment instead, and the plist is `0600` either way.
- **`--protocol http2`.** On a phone hotspot QUIC failed to reach the edge and `cloudflared` did not fall back for more than two minutes (tunnel 530, [0039](0039-gateway-approval-keepalive-and-wait-clamp.md)). HTTP/2 is always passed, including in the environment-token form.
- **Two KeepAlive LaunchAgents** (`gateway service install | uninstall | status`): `com.onebrain.gateway` (`onebrain gateway run`) and, when a tunnel is configured, `com.onebrain.gateway-tunnel`. Both have `KeepAlive` and `RunAtLoad`, log to `~/Library/Logs/onebrain/` (created `0600`; the pairing code is never written there, [0040](0040-gateway-pre-tunnel-exposure-hardening.md)), and are written `0600`. `install` is idempotent and restarts the gateway, which is how a changed `public_url` (allowed hosts are fixed at startup) takes effect. It refuses a `gateway.yml` that names no vault, because a launchd job starts in `/`, and refuses to install when something else already answers on the port.
- **Which `onebrain` the agent runs.** `current_exe()` (the build that ran `service install`) is used, except that a stable `PATH` entry such as the Homebrew bin path wins when it resolves to the same file. A chosen path under `/Cellar/` prints a warning to re-run `service install` after `brew upgrade`. Without this rule the agent would either pin a version-specific Cellar path or silently run a different build from the one the operator just installed.
- **Describe, do not roll back, on a partial failure.** If the gateway agent loads but the tunnel agent fails, `install` returns an error that names exactly what is left in place and how to finish or remove it, rather than silently unloading a working gateway. `uninstall` never touches tokens or config.
- **macOS only for now.** On other systems `gateway service` reports "not supported yet on <os>", and the docs show running `gateway run` and `cloudflared` under the operator's own supervisor. `gateway run` itself, and `gateway tunnel setup`, work on Linux and Windows.
- **`onebrain doctor` gains a Gateway section** whenever a `gateway.yml` exists: config, `public_url`, both agents (including a program path that no longer exists), local endpoint, tunnel (a failed network probe is a warning), auth store, Telegram, approval wait, and a warning when the vault is under `~/Documents`, `~/Desktop` or iCloud Drive, which a background agent may not be allowed to read.

## Consequences

- Setup is a few manual steps in the Cloudflare dashboard plus three commands. The cost is a Cloudflare account and a domain on it.
- After installing from Homebrew, re-run `gateway service install` so the agent stops pointing at an older or development build. Re-running it also drops any pending approval (the gateway restarts).
- The service logic is tested against a fake launcher; real launchd, the live tunnel and the real connector were verified by hand on a Mac.
- Linux (systemd) and Windows services are not provided.
- A guided single command with a choice of connection (Cloudflare with an owned domain, Tailscale Funnel, ngrok, a quick tunnel, local only) is the planned follow-up. Any alternative must first be checked against the `Host` guard.

## Alternatives considered

- **Create the tunnel and DNS from the CLI** with `cloudflared tunnel login` / `create` / `route`. Rejected for v3.5.0: it needs a `cert.pem` and ties the feature to `cloudflared`'s management CLI. Kept as an option for the guided setup.
- **Pass the token on the command line or in the plist environment by default.** Rejected, because a token on the command line is visible in the process list, while a file is not (considered while writing this ADR). The plist environment is kept only as the fallback for a `cloudflared` without `--token-file`.
- **Roll back on partial failure.** Rejected: unloading a working gateway on a tunnel error hides the actual state from the operator.
- **launchd for the gateway only, and a hand-run tunnel.** Rejected: a tunnel that does not restart makes the phone path as fragile as before.
- **Cloudflare quick tunnels.** Used for the smoke test only: the URL changes on every restart, so no connector could keep working.
