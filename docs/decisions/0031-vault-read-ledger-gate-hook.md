# 0031 — Vault-read ledger gate hook: deny only repeat-unchanged reads, fail-open, default off

- **Status:** accepted
- **Date:** 2026-07-11

## Context

Agents read vault notes with the host's `Read` tool constantly — a path that bypasses OneBrain entirely: uncapped, un-ledgered, invisible to `token gain`. Re-reading an unchanged 15k-token note three times in a session costs full price three times. RTK solves the analogous problem by rewriting Bash commands in a PreToolUse hook, but that mechanic doesn't transfer: a PreToolUse hook can edit a tool's *input*, not change which tool runs — `Read` cannot be turned into an MCP call. Routing nothing means leaving the largest unmetered token sink on the table; gating everything risks breaking the agent's most basic operation.

## Decision

- **Gate only the pure waste.** PreToolUse hook on `Read` for vault `.md` paths calls `onebrain token check <path>` (single-word verb, domain-command pattern like `checkpoint stop` — no new `hook` namespace). First-time read or changed content hash → **allow, untouched** — the common case has zero compat surface. Repeat read of an unchanged doc → **deny** with a machine-readable reference: `sent_earlier`, hash, and the recovery instruction (`onebrain search get <path> --force`). The ~50-token deny reason is the receipt the agent must receive anyway; it replaces a ~15k-token re-inline.
- **Fail-open is a hard rule.** Exit protocol: 0 = allow, 2 = deny-with-reference; any error, unresolvable session token, or daemon non-response beyond ~200 ms → allow. Hook timeout 5 s. A broken hook must never block file access. Every fail-open emits a `hook_failopen` gain event so `doctor` surfaces a dead daemon instead of silent degradation (the RTK SA-2025-RTK-001 lesson inverted: absence of a working rule must never widen permissions — here it must never *narrow* access either).
- **Three bypass layers:** `token_optimization.read_hook: off | ledger` in `onebrain.yml`; `ONEBRAIN_HOOK_BYPASS=1` per session; per-call `--force` per the deny reason.
- **Default `off` in the product for v3.4.10;** enabled (`ledger`) on the maintainer's own vault as a field test. `onebrain token discover` — which scans host session transcripts for direct vault reads that bypassed OneBrain and estimates the missed savings — is the measurement instrument; its data decides whether v3.4.11 flips the default.
- Plugin-side registration lives in the vault plugin (separate repo, own version bump); the CLI ships only the `token check` verb over the existing daemon ledger route.

## Consequences

- The unmetered blind spot closes where it hurts (repeats) with near-zero risk where it doesn't (first reads untouched, everything fail-open, three escapes).
- Default-off costs adoption speed but buys evidence: the flip decision will be made on gain data, not intuition.
- The hook adds one warm-daemon HTTP round-trip per vault `Read` (~ms against the read it may replace); on machines without a running daemon it degrades to a no-op by design.
