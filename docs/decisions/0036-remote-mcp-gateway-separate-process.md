# 0036 — Remote MCP gateway: a separate `onebrain gateway` process in the same binary

- **Status:** accepted
- **Date:** 2026-10
- **Follows:** [0019](0019-native-mcp-server-staged-qmd-cutover.md) (the stdio MCP server this sits beside) and [0033](0033-per-vault-daemon-slots.md) (the per-vault daemons it routes search through).
- **PRs:** [#399](https://github.com/onebrain-ai/onebrain-cli/pull/399) (rmcp 3.0.1 baseline), [#401](https://github.com/onebrain-ai/onebrain-cli/pull/401) (skeleton + Brain pack), [#408](https://github.com/onebrain-ai/onebrain-cli/pull/408) (first write tool).
- **Design:** "OneBrain Remote MCP Gateway — Design Spec" (v3.5 Gateway epic; the spec is not public).

## Context

The goal of v3.5 is to reach a vault from the Claude app on a phone while the Mac stays at home. `onebrain mcp` is stdio and single-vault: it is spawned by a local harness and cannot be reached by a remote client. A remote client needs an HTTP endpoint, authentication, a way to ask a human before anything is written, and a way to get through the home network without opening router ports.

Where that endpoint lives was the first choice: inside the existing daemons or `onebrain serve`, inside `onebrain mcp`, or as its own process.

## Decision

**Add `onebrain gateway` as its own long-lived process, built into the same `onebrain` binary.** It serves MCP over **streamable HTTP** using `rmcp` (pinned `=3.0.1`, protocol `2026-07-28`, stateless/sessionless), mounted at `/mcp`, and is the only surface meant to be reachable from outside.

- **Loopback bind only.** The gateway binds `127.0.0.1` with no `--bind` flag and no config key. Remote reachability comes exclusively from an outbound tunnel in front of it ([0042](0042-gateway-tunnel-and-launchagent-service.md)), never from a wider bind.
- **Machine-level config.** `~/.onebrain/gateway.yml` plus state under `~/.onebrain/gateway/`, not `onebrain.yml`, because one gateway spans several vaults. The `vaults:` name-to-path map is the first multi-vault registry in the codebase; tool calls pick a vault by name, falling back to `default_vault`.
- **Zero-config still works.** With no `gateway.yml`, `gateway run` inside a vault serves that vault through the normal env / walk-up resolution.
- **The Brain pack only.** `capabilities`, `brain_search`, `brain_get`, `brain_tasks` (read) and `brain_capture` (writes an inbox note, policy-gated, [0038](0038-gateway-policy-engine-and-human-approvals.md)). The `developer`, `files` and `mac` packs are listed by `capabilities` with `enabled: false` and have no tools.
- **Search goes through the per-vault daemon.** `brain_search` always routes through the warm daemon from [0033](0033-per-vault-daemon-slots.md). A long-lived multi-vault process must never take a per-vault exclusive engine lock itself, or one vault's request would starve the others.
- **Local clients are untouched.** `onebrain mcp` (stdio) keeps its tool set and behaviour.

## Consequences

- A second long-running process to supervise, hence the service work in [0042](0042-gateway-tunnel-and-launchagent-service.md). Foreground `gateway run` is still the primary form on Linux and Windows.
- The exposed attack surface is one small, separately audited HTTP server instead of the daemons, which stay loopback-only and unchanged.
- The gateway can crash or restart without taking local search with it, and the reverse.
- The OAuth resource server is hand-written axum middleware in front of `/mcp` ([0037](0037-gateway-oauth-authorization-server-and-pairing.md)): rmcp 3.0.1 provides client-side OAuth only.
- `gateway.yml` is read once at startup; most changes need a restart (`gateway service install` restarts).

## Alternatives considered

- **Add routes to the warm daemon or `onebrain serve`.** Rejected: both are per-vault and loopback-only by design; making them the internet-facing surface would couple their lifecycle and trust model to a multi-vault, authenticated service.
- **Make `onebrain mcp` speak HTTP.** Rejected: it is single-vault, spawned per session by a harness, and has no notion of clients, tokens or approvals.
- **A hosted relay instead of a local gateway.** Out of scope for v3.5: it needs infrastructure and a trust model the project does not have yet.
