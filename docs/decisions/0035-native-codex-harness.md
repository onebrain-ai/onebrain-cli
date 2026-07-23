# ADR 0035: Native Codex harness and managed plugin installation

## Status

Accepted — 2026-07-23.

## Decision

Codex is a first-class `Harness` and scheduled skills may select it per entry.
An omitted `harness` remains Claude for backward compatibility. Command-mode
entries reject `harness`.

Codex skills use `$onebrain:<skill>` and execute with `codex exec`,
`--sandbox workspace-write`, `--skip-git-repo-check`, `--ephemeral`, and
`-C <vault>`. Model and JSON flags are forwarded only when requested.

Plugin management is explicit opt-in. `onebrain plugin install --harness codex`
registers the vault marketplace, runs `codex plugin add onebrain@onebrain`,
enables only `features.hooks` and `features.multi_agent`, and writes two
matching records atomically: the vault-local `.codex/onebrain-plugin.json`
marker and a receipt under `CODEX_HOME/onebrain-managed/` bound to the vault's
canonical path. A repository can carry a forged local marker, so later update
automation and unattended hook-trust bypass require both records and an
`installed` receipt. A pending receipt authorizes uninstall cleanup only.
Installation failures remove partial plugin state and restore the previous
Codex config. Config rewrites preserve its existing permissions (new managed
files are owner-only on Unix), and Windows replacement uses the native
replace-existing primitive rather than deleting the destination first. A
failed compensating plugin removal retains a pending receipt so explicit
uninstall can finish cleanup without authorizing hook-trust bypass. Dry-run
changes no Codex-global state.

Codex 0.145 names the installation verb `plugin add`, not `plugin install`;
retry output uses the executable command rather than mirroring OneBrain's verb.

Session identity is the hook payload's complete `session_id`, hashed to a stable
16-character alphanumeric token. Using the complete value avoids collisions
between UUIDs that share a prefix. The same identity must be supplied to
SessionStart and Stop/checkpoint hooks, and SessionStart injects the derived
token for wrapup, so concurrent Codex chats in one vault never share checkpoint
or session-log state.

Hook trust bypass is reserved for unattended OneBrain-managed runs. Interactive
Codex sessions retain the normal `/hooks` trust flow.
