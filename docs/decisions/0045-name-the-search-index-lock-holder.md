# 0045 — Name the search-index lock holder

- **Status:** accepted
- **Date:** 2026-10
- **Follows:** [0022](0022-honest-search-lock-errors.md) and [0023](0023-warm-daemon-mcp-search.md) (revisits both: the lock error is still honest, but now says who holds the lock).
- **PRs:** [#440](https://github.com/onebrain-ai/onebrain-cli/pull/440); issue #426.

## Context

The search collection's lock is a redb exclusive open, so a process that loses the race learns only that someone holds it (`EngineBusy`, ADR 0022). After an upgrade that someone is usually an `onebrain mcp` from the old binary, still running inside an open agent session, or a daemon left behind by one. "Locked by another process" gives the user nothing to act on: they have to find the process and work out whether it is old.

## Decision

- **A holder record next to the lock.** As soon as the engine holds the collection lock it writes `<collection>/.collection.lock.holder`, JSON `{pid, version, exe, role, started}`, by temp file and rename. It removes the record before releasing the lock, and only if the record still names its own pid, so a stale holder cannot delete the next holder's record. If the write fails, any older record is removed (we hold the lock, so it is stale). The record is advisory: the lock stays the redb open, and a missing or unreadable record only means the holder cannot be named.
- **Roles are set once at dispatch.** The CLI records what kind of process this is (`mcp`, `daemon`, `gateway`, `serve`, otherwise `cli`) from the command it dispatched, first call wins.
- **When a loser names the holder.** Only when the record's pid is alive right now (on platforms that can probe it), the pid is not the loser's own, and the role is one we write. The version is shown only if it is short printable ASCII.
  - Same version: "… in use by onebrain mcp 3.5.1 (pid N) — retry once it releases the lock".
  - Holder older than ours: "… — from before an upgrade, <hint>".
  - Newer, or a version that is not plain dotted numbers: "… — a different onebrain version, <hint>" (we do not guess "before an upgrade").
  - Hint by role: an `mcp` holder says to restart that agent session (Claude Code / Codex / Gemini); a `daemon` holder says `onebrain daemon stop --vault <vault>` when the vault is known (plain `onebrain daemon stop` inside the vault otherwise); anything else says to stop that process.
  - No record, a dead pid, our own pid, an unknown role, or a platform with no liveness probe: the generic "the search index is in use by another onebrain process; if you just upgraded, restart open agent sessions (Claude Code / Codex / Gemini)".
- **Where it shows.** The daemon's log, the daemon client's error (so the gateway log and routed CLI verbs), `search status` (text hint and `W_ENGINE_BUSY` warning; the holder is not in the JSON, so there is no wire change), `doctor` (the `search` and `lex-index` rows), and the direct CLI open path. The client-visible gateway `brain_search` error stays the sanitized "search backend unavailable — see gateway logs"; the holder appears only in the gateway log.
- **Upgrade docs.** Because the commonest cause is an upgrade, `docs/install.md` now tells users to restart open agent sessions and stop old daemons after upgrading.

## Consequences

- After an upgrade the error says which process to restart or stop, instead of "locked by another process".
- Holders from before v3.5.1 never wrote a record, so the first upgrade to 3.5.1 still prints the generic hint for them. The record helps from then on.
- A hard-killed holder leaves its record behind. The next holder overwrites it on acquisition, and a loser names a pid only while it is alive, so the remaining risk is a recycled pid that now belongs to an unrelated live process.
- The record is plain JSON in the data directory, with the holder's executable path; it is local to the machine and holds no credentials.
- Windows and Unix can probe pids; other targets always get the generic message.

## Alternatives considered

- **Write the holder into the lock file itself.** Rejected: the redb file is owned by redb.
- **Ask the daemon who holds the lock.** Rejected: the holder is often not a daemon, and the daemon may itself be the loser.
- **Always print the generic upgrade hint.** Rejected: it does not help when the holder is a current-version process, and it does not say which process to restart.
