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

**Not yet shipped** (later PRs in the v3.5 epic): authentication (OAuth 2.1 + pairing) and a remote tunnel. `capabilities` already lists the roadmapped `developer`/`files`/`mac` packs with `enabled: false` so a caller can see what's coming without probing for tools that don't exist yet.

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

## Security posture — no auth yet

**Loopback only.** The bind address is hard-coded to `127.0.0.1` — there is no `--bind` flag, no `$ONEBRAIN_BIND`-style escape hatch (unlike [`onebrain serve`](serve.md#containers--self-host--onebrain_bind)), and no config key to change it. This is deliberate: this build ships **no authentication layer** of any kind, so nothing should ever expose this port beyond the local machine.

Do not put this port behind a plain reverse proxy or port-forward it to another host. A remote tunnel and OAuth 2.1 + pairing (so a phone or another machine can reach your gateway safely) are planned for a later PR in the v3.5 epic — until then, `onebrain gateway run` is a localhost-only tool: a local MCP client that wants Streamable HTTP instead of stdio, or a testing/development target.
