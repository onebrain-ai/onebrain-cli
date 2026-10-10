# 0037 — Built-in OAuth 2.1 authorization server with device pairing

- **Status:** accepted
- **Date:** 2026-10
- **Follows:** [0036](0036-remote-mcp-gateway-separate-process.md).
- **PRs:** [#403](https://github.com/onebrain-ai/onebrain-cli/pull/403) (authorization + resource server), [#417](https://github.com/onebrain-ai/onebrain-cli/pull/417) (`resource` recorded on tokens), [#418](https://github.com/onebrain-ai/onebrain-cli/pull/418) (token / client management).
- **Design:** "OneBrain Remote MCP Gateway — Design Spec", section on auth.

## Context

Remote connector clients (the Claude app's custom connector) can talk to a server with no auth, but an endpoint that can write into a personal vault must not be open. The clients can only do the standard MCP / OAuth handshake: they cannot send custom headers during it, so a static header or an edge-level access gate in front of `/mcp` does not work for them. `rmcp` 3.0.1 ships OAuth for clients only, so there is no server-side auth to reuse.

## Decision

**The gateway embeds its own OAuth 2.1 authorization server and resource server, gated by a human pairing code.**

- **Discovery and registration.** RFC 9728 protected-resource metadata, RFC 8414 authorization-server metadata, and RFC 7591 dynamic client registration. Every client is **public**: `token_endpoint_auth_method` is always `none`, and no client secret is ever generated or stored. `application_type` `web` needs `https://` redirects; `native` needs a loopback redirect (RFC 8252).
- **Authorization code + PKCE `S256` only.** `plain` is rejected. The code is single-use and consumed on presentation whether or not the request is otherwise valid; replaying a used code revokes every token it produced.
- **Opaque tokens, not JWTs.** Codes, access and refresh tokens are 32 random bytes (OS CSPRNG, base64url) kept in a server-side store (`~/.onebrain/gateway/{clients,codes,tokens,pairing}.json`). The authorization and resource servers are the same process, so self-contained tokens buy nothing, and revocation becomes deleting an entry. No JWT or HMAC crate enters the dependency graph. Lifetimes: code 10 min, access 1 h, refresh 30 d.
- **Rotating refresh tokens with reuse detection.** Each refresh invalidates the presented token and issues a new pair. Presenting an already-rotated refresh token is treated as theft and revokes the whole token family. A `client_id` supplied on refresh must match the client the token was issued to.
- **Device pairing.** The consent page needs a pairing code that is readable only on the Mac (`onebrain gateway pair [--rotate]`). Five consecutive wrong codes lock **every** pairing-code check for 60 s, with one global counter rather than per client or per address, because there is exactly one pairing code and one human. The code is not rotated after each approval; it is the single-user standing device credential and `pair --rotate` changes it on demand.
- **RFC 8707 resource binding (recorded).** Every token pair records the resource (`{issuer}/mcp`) it was issued for. Enforcing it in the bearer check is **not** done yet (one resource exists today); it is tracked in [#416](https://github.com/onebrain-ai/onebrain-cli/issues/416). Tokens minted before the field existed carry none and keep working.

## Consequences

- Revocation is exact and immediate: the gateway re-reads the store on every request, so `gateway tokens revoke` / `clients remove` ([0041](0041-gateway-auth-store-lock-and-toctou-recheck.md)) take effect on the client's next call with no restart.
- **Deliberate trade-off, onboarding lockout.** Because the counter is global, anyone who can reach `/authorize` through a tunnel can keep new pairing locked out by sending wrong codes. This is fail-safe (a lockout refuses, never permits), existing clients and `/mcp` are unaffected, and the remedies are pausing the tunnel or restarting the gateway. A per-address counter was rejected because an attacker with many addresses would get unlimited guesses at the one code.
- Registration needs no credentials, so it is bounded ([0040](0040-gateway-pre-tunnel-exposure-hardening.md)).
- **Client ID Metadata Documents (CIMD) are not implemented.** They need an SSRF-safe fetch of a client-supplied URL, which is its own design; every client uses `/register`.
- The wire contract is hand-written and must be re-verified when `rmcp` or the MCP spec moves.

## Alternatives considered

- **No auth.** Rejected: the endpoint can write into the vault.
- **A static bearer header or an edge access gate on `/mcp`.** Rejected: connector clients cannot send custom headers in the handshake. An edge gate is usable for admin paths only.
- **JWT access tokens.** Rejected for the reasons above (same-process issuer and verifier, revocation, dependency graph).
- **Confidential clients with a secret.** Rejected: `/register` accepts only `token_endpoint_auth_method: none` and rejects any other method (a test covers `client_secret_basic`), and discovery advertises only `none`. The further argument that a stored secret would add nothing against someone who can read the user's files is our own reasoning (considered while writing this ADR).
- **Per-address pairing lockout.** Rejected (see trade-off above; the attacker-with-many-addresses argument is also from the design notes on the onboarding lockout, not from a measurement).
