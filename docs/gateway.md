# OneBrain Gateway — `onebrain gateway run`

`onebrain gateway run` starts a loopback Streamable HTTP MCP server serving a read-only, multi-vault tool pack — the v3.5 Gateway epic's first shipped piece. This page covers what the v3.5 **skeleton** ships today: the `gateway.yml` schema, its defaults, zero-config behavior, and the current (deliberately narrow) security posture. See [`docs/reference/mcp.md`](reference/mcp.md#gateway-streamable-http) for the tool-by-tool reference and [ADR 0019](decisions/0019-native-mcp-server-staged-qmd-cutover.md) for the wider native-MCP architecture this sits alongside.

```bash
onebrain gateway run              # bind the configured (or default 7717) port
onebrain gateway run --port 0     # let the OS assign an ephemeral port
```

- Runs in the foreground until Ctrl-C.
- Binds **`127.0.0.1` only** — see [Security posture](#security-posture--no-auth-yet) below.
- The bound URL prints once to stdout on startup: `gateway listening on http://<bound-addr>/mcp`.

## What this skeleton ships

- One loopback HTTP endpoint (`http://127.0.0.1:<port>/mcp`), Streamable HTTP, protocol `2026-07-28` pinned as the negotiation fallback.
- Four read-only tools — the **Brain pack**: `capabilities`, `brain_tasks`, `brain_get`, `brain_search`.
- **Multi-vault**: any vault named in `gateway.yml`'s `vaults:` map is reachable by name, per tool call — the first part of the codebase that tracks more than one vault at a time.
- `brain_search` always routes through the warm per-vault daemon rather than opening a direct search engine itself: a long-lived, multi-vault gateway process must never take an exclusive per-vault engine lock, or one vault's request would starve every other vault's request against the same gateway.
- **OAuth 2.1 authentication**: `/mcp` requires a Bearer access token — see [Authentication](#authentication) below. The `/.well-known/*` discovery documents and `/register`/`/authorize`/`/token` stay reachable without one (a client with no token yet must be able to bootstrap OAuth before it has one).

**Not yet shipped** (a later PR in the v3.5 epic): a remote tunnel, so a phone or another machine can reach your gateway safely. `capabilities` already lists the roadmapped `developer`/`files`/`mac` packs with `enabled: false` so a caller can see what's coming without probing for tools that don't exist yet.

## Zero-config behavior

`~/.onebrain/gateway.yml` is entirely optional. With no file present:

- The gateway still starts, on the default port `7717`.
- Every tool call resolves its vault the normal way when `vault` is omitted: the same env (`$ONEBRAIN_VAULT`) / walk-up chain `onebrain mcp` and the CLI search verbs use, rooted at the gateway process's own working directory.

So a bare `onebrain gateway run` launched from inside a vault directory serves that one vault with zero configuration. The config file exists for the **multi-vault** case: naming several vaults by name, or pinning a default vault that differs from wherever the process happens to be launched from.

## `gateway.yml` schema

Machine-level config at `~/.onebrain/gateway.yml` — deliberately **not** per-vault (unlike `onebrain.yml`), because one gateway process spans multiple vaults.

| Key | Type | Default | Notes |
|---|---|---|---|
| `port` | number | `7717` | Loopback port `gateway run` binds when `--port` is omitted. `--port` on the command line always wins over this. |
| `default_vault` | path | unset | Vault served when a tool call omits `vault`. Unset falls through to `$ONEBRAIN_VAULT`, then walk-up from the gateway process's cwd — exactly like an explicit CLI `--vault` flag would win over both of those when it IS set. |
| `vaults` | map (name → path) | `{}` | Named vaults a tool call may select via its `vault` argument. An unknown name is a JSON-RPC `invalid_params` error listing the known names. |
| `public_url` | string | unset | The gateway's OAuth issuer base URL, for the still-unshipped remote tunnel (see [Loopback + no remote exposure yet](#loopback--no-remote-exposure-yet)). When set, `gateway run` advertises `public_url` as the issuer in every discovery document (`/.well-known/oauth-protected-resource`, `/.well-known/oauth-authorization-server`) and in the `/mcp` 401 `WWW-Authenticate` challenge, instead of `http://127.0.0.1:<bound-port>`. Must be a bare origin — `scheme://host[:port]` — with no path, query, or fragment (a single trailing `/` is trimmed automatically); `http://` is accepted only for a loopback host (`localhost`/`127.0.0.1`), every other host must use `https://`. `gateway run` validates this at startup and refuses to start on an invalid value (naming the `public_url` key in the error) rather than silently falling back to the loopback issuer. Setting this alone does not expose anything remotely — this build still binds `127.0.0.1` only. |

All keys are optional; a missing file behaves identically to an empty one.

Example naming two vaults with a default:

```yaml
port: 7717
default_vault: /Users/you/ob-1
vaults:
  personal: /Users/you/ob-1
  work: /Users/you/ob-work
```

A `brain_tasks`/`brain_get`/`brain_search` call passing `"vault": "work"` then serves `/Users/you/ob-work`; omitting `vault` serves `default_vault` (`/Users/you/ob-1`).

## Authentication

`/mcp` is an OAuth 2.1 resource server: every request needs `Authorization: Bearer <access-token>`, or it gets a `401` with a `WWW-Authenticate` header pointing at the discovery document below. Getting a token is a standard OAuth 2.1 authorization-code + PKCE flow, gated by a **device-pairing code** — the human-in-the-loop step that stands in for a client secret this authorization server deliberately never issues (see [Token semantics](#token-semantics)).

### The flow, in prose

1. **Discovery.** A client with no token GETs `{issuer}/.well-known/oauth-protected-resource` (RFC 9728 — also served at the `/mcp`-suffixed path, since `/mcp` is itself the protected resource) to learn its authorization server, then that server's `{issuer}/.well-known/oauth-authorization-server` (RFC 8414) to learn the `/register`/`/authorize`/`/token` endpoints and that only PKCE `S256` is supported (`plain` is rejected — OAuth 2.1 drops it entirely).
2. **Registration.** The client `POST`s `{issuer}/register` (RFC 7591) with its `redirect_uris` and an `application_type` of `"web"` (must use `https://`) or `"native"` (must use a loopback `http://localhost`/`http://127.0.0.1` redirect, RFC 8252 §7.3). Every client this authorization server mints is **public** — no client secret is ever generated or stored; `token_endpoint_auth_method` is always `"none"`.
3. **Authorize.** The client sends the user's browser to `GET {issuer}/authorize` with the standard `response_type=code`, `client_id`, `redirect_uri`, `code_challenge`/`code_challenge_method=S256`, and `state` parameters. This renders the **one human checkpoint** in the whole surface: a consent page showing the requesting application's name, requested scope, and the redirect target's host, with a single field — the pairing code shown on the gateway (see [Pairing](#pairing) below). A wrong code re-renders the same page with a generic "incorrect" notice (never which check failed); five consecutive wrong submissions lock pairing out for 60 seconds, even for a correct code submitted mid-lockout. A correct code mints a single-use authorization code and redirects back to `redirect_uri` with `code`, `state`, and `iss` (RFC 9207) query parameters.
4. **Token exchange.** The client `POST`s `{issuer}/token` with `grant_type=authorization_code`, the code, and its PKCE `code_verifier`. A valid exchange returns an opaque `access_token` + `refresh_token` pair; the code is consumed on presentation regardless of outcome, so it can never be redeemed twice.
5. **Calling `/mcp`.** Every request carries `Authorization: Bearer <access_token>` until it expires (see below), at which point the client exchanges its `refresh_token` for a fresh pair the same way (`grant_type=refresh_token`).

### Pairing

The device-pairing code is the credential a human types once, at `/authorize`, to approve a new client — the gateway's stand-in for "you, physically at this machine, said yes."

- **`onebrain gateway run`** mints the code on first run and prints it to stdout — the *only* place it is ever shown: never logged, never returned over any HTTP response (an automated test asserts this directly). The code is stable across restarts; running `gateway run` again does not rotate it.
- **`onebrain gateway pair`** prints the current code without starting the gateway — useful from a second terminal, or when the gateway is already running headless/backgrounded. `onebrain gateway pair --rotate` mints a fresh code in its place, immediately invalidating the old one (any client mid-pairing with the old code must restart `/authorize`).

### Token semantics

Every credential this authorization server mints — authorization codes, access tokens, refresh tokens — is a random **opaque** string (32 bytes from the OS CSPRNG, base64url-encoded), never a signed/self-describing token like a JWT. Opaque tokens give exact revocation for free: revoking one is deleting a store entry, not maintaining a denylist alongside a signature scheme.

| Credential | Lifetime | Notes |
|---|---|---|
| Authorization code | 10 minutes (600s) | Single-use — consumed the moment it's presented at `/token`, whether or not the rest of the request is valid. A later replay of an already-used code revokes every token that code ever produced. |
| Access token | 1 hour | Presented as `Authorization: Bearer <token>` on every `/mcp` call. |
| Refresh token | 30 days | **Rotates on every use** (RFC 6749 §6 / OAuth 2.1 §4.14.3): each `grant_type=refresh_token` exchange invalidates the presented token and mints a new pair. Presenting an already-rotated refresh token again is treated as a leaked-token signal — the entire token family it descended from (every access/refresh token minted since the original code exchange) is revoked immediately, not just the reused token. |

### Not yet shipped

**Client ID Metadata Documents (CIMD)** — letting a client identify itself by a `https://` URL instead of going through `/register` — are deferred to a follow-up PR; landing CIMD safely requires an SSRF-safe fetch of that URL (the AS would otherwise follow a client-supplied URL from inside the gateway process), which is its own piece of design work. Every client today registers via `/register` (RFC 7591) instead.

### Loopback + no remote exposure yet

The bind address is still hard-coded to `127.0.0.1` — there is no `--bind` flag, no `$ONEBRAIN_BIND`-style escape hatch (unlike [`onebrain serve`](serve.md#containers--self-host--onebrain_bind)), and no config key to change it. OAuth authenticates *who* may call `/mcp`; it does not by itself make exposing this port beyond the local machine safe — do not put it behind a plain reverse proxy or port-forward it to another host. A remote tunnel (so a phone or another machine can reach your gateway through the same pairing flow) is planned for a later PR in the v3.5 epic — until then, `onebrain gateway run` is a localhost-only tool: a local MCP client that wants Streamable HTTP instead of stdio, or a testing/development target.
