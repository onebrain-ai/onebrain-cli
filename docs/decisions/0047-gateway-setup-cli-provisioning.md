# 0047 — Guided gateway setup: CLI-provisioned Cloudflare tunnel, foreground Quick Tunnel

- **Status:** accepted
- **Date:** 2026-10
- **Partially supersedes:** [0042](0042-gateway-tunnel-and-launchagent-service.md) for *provisioning* only (its "no `cert.pem`, no `cloudflared` management commands" decision). The runtime of 0042 (token file, `--protocol http2`, two KeepAlive LaunchAgents) is unchanged apart from the `--url` addition below.
- **Follows:** [0040](0040-gateway-pre-tunnel-exposure-hardening.md) (the `Host` guard every connection is checked against).
- **Issue:** #423 (v3.5.2 "Guided gateway setup").
- **Design:** "v3.5.2 — Guided gateway setup · design" (decisions D1–D10).

## Context

Getting from an installed `onebrain` to a working phone connector took about nine manual steps: create a tunnel and a public hostname in the Cloudflare Zero Trust dashboard, copy the token, run `gateway tunnel setup`, hand-write `default_vault`, run `gateway service install`, run `gateway pair`, type the URL on the phone. The owner found this cumbersome in the v3.5.0 live test. 0042 had deliberately avoided `cloudflared tunnel login` and its `cert.pem`, and listed CLI provisioning and a quick tunnel as options for a later guided setup.

## Decision

- **One wizard, `onebrain gateway setup`:** vault → connection → Telegram (optional) → service → finish screen (connector URL; on a terminal also the pairing code and a QR code). Telegram is asked before the service step, so a single install picks up the `telegram:` block. Every step detects the current state and Enter keeps it. A step failure, Ctrl-C or end of input stops the wizard, lists the finished steps and exits non-zero. Finished steps are not rolled back, and re-running is safe.
- **Own domain, provisioned from the CLI.** `cloudflared tunnel login` (skipped when a `cert.pem` exists) → reuse the tunnel named `onebrain-<host with dashes>` (`tunnel list --name … -o json`) or `tunnel create` → `tunnel route dns <name> <host>` → `tunnel token <name>` → the existing `~/.onebrain/gateway/tunnel.token` (0600) plus `public_url` and `tunnel_ingress: local` in `gateway.yml`. Nothing is written until the route succeeded. `--overwrite-dns` is passed only after an explicit "yes" in that run. Success is trusted only when the FQDN `cloudflared` reports equals the hostname, because for a hostname outside the login's zone `cloudflared` appends the zone and exits 0. That case is reported as a stray record to delete, and nothing is written. A route call that timed out may still have created the record, so the error says re-running is safe (the next run sees it as already routed).
- **`tunnel_ingress: local` (runtime change).** A CLI-created tunnel is locally managed: it has no ingress rules in the dashboard, so `cloudflared tunnel run --token-file …` alone answers 503. When `tunnel_ingress: local` is set, the tunnel LaunchAgent and the printed manual command append `--url http://127.0.0.1:<port>`, which needs a fixed `port` (not `0`). Without the key, dashboard tunnels render exactly as before. Storing a dashboard token (`gateway tunnel setup`, or the wizard's "I already have a tunnel token") clears a leftover `local`.
- **Quick Tunnel (`*.trycloudflare.com`) is a foreground test mode only.** The wizard starts `cloudflared tunnel --no-autoupdate --protocol http2 --url http://127.0.0.1:<port>`, reads the URL from its output (stderr on cloudflared 2026.10.0), and runs the gateway in the same process with that URL as an in-memory `public_url` override. The host guard and the OAuth issuer both derive from it. On a fresh machine the chosen vault is also passed in memory. `gateway.yml`, the token file and the LaunchAgents are never written. Only a URL that matches `https://<one label>.trycloudflare.com` exactly can become the override, and it passes the same validation as a configured `public_url`. The wizard refuses when the port is busy (for example, the service is running), and cloudflared is killed when the gateway stops.
- **Restart only when needed.** The service is reinstalled (which restarts the gateway and denies pending approvals) when this run changed `gateway.yml` or the token file, or when the agents' plists no longer match a fresh render. The plists do not contain `public_url`, the port or the vault, so a plist comparison alone would miss a config change.
- **Secrets.** The tunnel token is never printed, logged or put in an error message. The pairing code appears only on a terminal. The QR code contains the connector URL only.

## Probe results (2026-10-11, cloudflared 2026.10.0)

- Quick Tunnel: the URL is printed on **stderr only**, in a banner line (`… INF |  https://<name>.trycloudflare.com  |`) about 4–5 s after start, after an `INF Requesting new quick Tunnel on trycloudflare.com...` line. The `Host` header reaches the origin unchanged. In one run a fresh hostname did not resolve for over 40 s, so the wizard says the URL can take up to a minute to start working and runs an advisory reachability check that never blocks the gateway.
- The CLI-provisioning output samples the parsers are tested against were written before the owner-login probe and are replaced with captured output when that probe runs. They live in one shared fixtures module, so a correction reaches the wizard's tests too.

## Consequences

- A fresh Mac reaches a working connector with one command and no Cloudflare dashboard. The cost is a `cert.pem` in `~/.cloudflared/`, which can manage DNS and tunnels for the chosen zone, created by `cloudflared` itself and protected like any other `cloudflared` login.
- The wizard never deletes a tunnel or a DNS record. Removing a test tunnel is `cloudflared tunnel delete <name>` plus the CNAME in the dashboard.
- Off macOS the wizard provisions the tunnel and `gateway.yml`, then prints the commands to keep running (the service step stays macOS-only, #432).
- A Quick Tunnel cannot be used while the service runs on the same port: `gateway service uninstall` first, `gateway service install` afterwards.

## Alternatives considered

- **Persist the Quick Tunnel URL and restart on change.** Rejected: the URL changes on every start, so it would need rewrite-and-restart machinery and would still break every saved connector.
- **Keep the dashboard flow only.** Rejected: it is the nine-step path this release removes. It remains available as connection 2 and as `gateway tunnel setup`.
- **Always pass `--url` to `cloudflared`.** Rejected: it would change the program of every existing dashboard-tunnel LaunchAgent. The `tunnel_ingress` key keeps dashboard tunnels unchanged.
- **Tailscale Funnel, ngrok, non-interactive flags.** Out of scope for v3.5.2. Any later connection must first be checked against the `Host` guard.
