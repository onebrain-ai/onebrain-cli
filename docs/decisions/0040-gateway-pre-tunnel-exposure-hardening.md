# 0040 — Pre-tunnel exposure hardening

- **Status:** accepted
- **Date:** 2026-10
- **Follows:** [0037](0037-gateway-oauth-authorization-server-and-pairing.md), [0038](0038-gateway-policy-engine-and-human-approvals.md).
- **PRs:** [#417](https://github.com/onebrain-ai/onebrain-cli/pull/417); tracking issue #404.
- **Design:** "OneBrain Remote MCP Gateway — Design Spec", section on security; release rule that no tunnel ships before this lands.

## Context

Everything reachable on loopback is reachable by anyone once a tunnel forwards the public hostname to it. Several protections that are fine on loopback stop being fine: DNS rebinding, cross-site form posts at the consent page, an unauthenticated registration endpoint, a pairing code that lands in a log file, and an operator page that must never be answerable from the internet.

## Decision

Land these before any tunnel work, all on every route, loopback included:

- **`Host` allowlist.** Every route requires a loopback `Host` (`localhost`, `127.0.0.1`, `[::1]`, any port) or the host of `public_url`; otherwise `403`. A malformed or missing `Host`, or an empty one such as `Host: :80`, is `400`. `public_url` is validated at startup (bare origin, no path, query, fragment or userinfo; `http://` only for loopback) and `gateway run` refuses to start on a bad value. `rmcp`'s own `allowed_hosts` is configured from the same list.
- **`Origin` check on state-changing requests.** Outside `/mcp`, a `POST`/`PUT`/`PATCH`/`DELETE` with an `Origin` must equal the issuer origin. `Origin: null` passes only with `Sec-Fetch-Site: same-origin`. No `Origin` passes (non-browser clients). `/mcp` is exempt, since it needs a bearer token.
- **Consent-page CSRF without a token.** Three layers in order: fetch metadata (`Sec-Fetch-Site` other than `same-origin` / `none` is refused before the pairing code is looked at, so it does not spend lockout budget), the `Origin` rule above (the page sends `Referrer-Policy: same-origin`), and the pairing code in the form body, which a forging page cannot know. The page labels the client name "provided by the app, not verified", isolates it in `<bdi>`, and shows the full redirect address.
- **`/approvals` is loopback-only.** It answers only on a loopback `Host`, and is refused when any reverse-proxy header (`Forwarded`, `X-Forwarded-For`, `X-Forwarded-Host`, `X-Real-IP`, `CF-Connecting-IP`) is present, before the pairing code is checked. Away from the Mac, approvals go through Telegram.
- **Registration limits.** `POST /register` takes no credentials, so: 10 registrations per 60 s overall, 50 registered clients in total, `client_name` at most 100 characters, each redirect URI at most 2048 bytes.
- **Body limits.** `/register`, `/token` and `/authorize` bodies are capped at 64 KiB (`413`).
- **Pairing code is printed only to a terminal.** `gateway run` prints it only when stdout is a TTY; otherwise it prints a pointer to `onebrain gateway pair`. A launchd log therefore never contains it.

## Consequences

- **A tunnel must forward the original `Host`.** A tunnel rewriting `Host` to `localhost` would have its traffic accepted as loopback. The `/approvals` proxy-header check catches the common case, but a proxy that rewrites `Host` and strips every forwarding header is undetectable from inside the gateway; the pairing code and its lockout are the backstop. The docs forbid `Host` rewriting. This was a breaking change for anyone who already ran a tunnel by hand.
- **Deliberate trade-offs, stated in the user docs:** registration exhaustion (anyone can fill the 50-client cap; existing clients and `/mcp` are unaffected; recover with `clients list` / `clients remove`), and the onboarding lockout from [0037](0037-gateway-oauth-authorization-server-and-pairing.md).
- The only reliable proof that a request reached this gateway through the tunnel is the issuer probe in `tunnel status`.
- Verified in a real browser (Safari) on a scratch home, and later through the live tunnel: forged `Host` / `Origin` and `/approvals` via the tunnel are `403`, and no foreign-`Origin` server client was blocked.

## Alternatives considered

- **A synchroniser CSRF token.** Rejected: the pairing code in the body already is a secret a forging page cannot know, and fetch metadata plus `Origin` give a cheap earlier refusal.
- **Per-address rate limits.** Rejected: addresses are not a stable identity behind a tunnel, and a per-address limit multiplies the guesses available against the single pairing code.
- **Answering `/approvals` through the tunnel with the pairing code.** Rejected: it would put the approval gate behind the same credential an internet caller can attack.
- **Cloudflare Access on every path.** Not possible for `/mcp`: connector clients cannot send the headers it needs.
