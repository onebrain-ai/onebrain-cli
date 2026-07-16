# 0033 — Per-vault daemon slots: multi-vault warm daemons on one machine

- **Status:** accepted
- **Date:** 2026-07-16
- **Follows:** [0032](0032-self-healing-daemon-fallback.md) (which named "true multi-vault daemons" as the #230 follow-up) and [0023](0023-warm-daemon-mcp-search.md) (the warm-daemon model).

## Context

Through v3.4.12 the daemon was a **machine singleton**: one set of runtime files (`~/.onebrain/run/daemon.{json,pid,lock,log}`) bound to ONE vault, on a fixed port (6789 default). The data layer was already per-collection isolated (each vault's cache dir is keyed on `short_path_hash(canonical vault path)`); only the daemon *discovery / lifecycle / port* layer was single-slot.

That single slot made two vaults' concurrent sessions **thrash**. The MCP `discover()` path is an active lifecycle owner: on a vault mismatch it STOPPED the running daemon and (via `ensure_running`) started a fresh one for its own vault. So two vaults ping-ponged — session A starts A's daemon, session B stops it and starts B's, A stops that and starts A's… — each restart dropping the engine and re-opening redb. The cross-vault comparison machinery (`VaultDecision` / `VaultExpectation` / `vault_decision`) existed only to drive this "restart the machine's one daemon for my vault" behaviour.

## Decision

**Give each vault its own daemon slot, keyed on the vault-path hash — the exact primitive that already keys the per-vault search-collection cache dirs.**

- **Slot identity = `short_path_hash(canonical vault path)`.** Runtime files become `daemon-<hash>.{json,pid,lock,log}`. A vault-less daemon uses a stable `daemon-none.*` sentinel slot. One shared resolver (`daemon_client::resolve_slot` / `slot_paths_for_id` / `SlotPaths`) is the single source of truth for slot paths — used by both the daemon (writer) and clients (readers), closing the pre-existing "two `run_dir()`s kept in lockstep by comment" drift hazard.
- **Ephemeral ports.** Two daemons can't share a fixed port, so each binds `:0` (OS-assigned) and publishes the real port in its slot json. Discovery / serve / mcp / status all read the port from the slot; **nothing assumes 6789**. `$ONEBRAIN_DAEMON_PORT` remains a per-invocation override (single-daemon convenience / tests).
- **No cross-vault comparison, no stealing.** Each caller resolves ITS vault's slot (`resolve_slot(expected_vault)`) and reads only that `daemon-<hash>.json`. `discover()` restarts a daemon only on a **same-slot version skew** (its own vault, wrong wire version); it never stops another vault's daemon. `ensure_running` spawns a *sibling* for its slot rather than displacing anyone. The `VaultDecision`/`VaultExpectation` machinery is removed; its "Unresolvable is conservative" safety moves into `SlotResolve::Unresolvable` (a caller whose vault won't canonicalize gets no slot → routes direct / refuses to spawn), and a defensive same-vault check remains in `record_is_our_vault` (guards a hash collision or a stale record from a moved vault).
- **Per-slot start lock.** The `O_EXCL` start guard is per-slot, so two *different* vaults starting concurrently never serialize; *same-vault* concurrent starts still mutually exclude to "exactly one daemon". A contended loser polls the slot's PID file briefly so it reports the winner's real PID, not a bare `pid 0`.
- **Slot-aware `status` / `stop` + doctor.** `daemon status` enumerates every `daemon-*` slot and reports each running daemon; `daemon stop` gains `--vault` (one slot) and `--all` (every slot). A doctor check enumerates running daemons and flags stale/wedged slots. `daemon stop --vault X` keys on `canonical(X)` directly (not re-validating X is still a vault), preserving start/stop symmetry when X's `onebrain.yml` was removed after start.

## Consequences

- **`daemon status --json` shape change (breaking):** a single `{running, pid}` object → a `{ "daemons": [ … ] }` list, since multiple daemons can run. No plugin/CI consumer parses `daemon status --json`; the human text output keeps "daemon not running" and per-daemon blocks.
- **Upgrade window (transient):** a pre-v3.4.13 machine-wide `daemon.json` daemon still holds a vault's redb lock, but new sessions read slot files and don't see it, so a same-vault slot daemon can't open the engine and serves engine-less (503) until the old one idles out (≤30 min). It self-heals; `daemon stop --all` ends it immediately. Both `daemon status` and `doctor` surface a live legacy daemon (with the retire hint) so the window is visible + actionable rather than a silent "not running". A dead legacy record is inert (the `daemon-` prefix excludes it from slot enumeration).
- **`serve`** reuses/starts only ITS vault's slot daemon and prints that daemon's ephemeral-port URL; two different-vault daemons coexisting is now correct, not a collision (revising 0032's "restart the machine's single daemon" stance).
- Full concurrent per-vault daemons are now the shipped model, not the deferred one 0023/0032 referenced.
