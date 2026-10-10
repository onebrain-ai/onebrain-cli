# 0041 — Cross-process auth-store lock, post-mint re-check, and `rust-version 1.89`

- **Status:** accepted
- **Date:** 2026-10
- **Follows:** [0037](0037-gateway-oauth-authorization-server-and-pairing.md).
- **PRs:** [#418](https://github.com/onebrain-ai/onebrain-cli/pull/418); tracking issue #406.

## Context

`gateway tokens revoke` and `gateway clients remove` are separate CLI processes that edit the same JSON files (`clients.json`, `codes.json`, `tokens.json`, `pairing.json`) as the running gateway. Every operation re-reads its file, modifies a copy and atomically renames it back, so with two processes the **last rename silently wins**: a revoke could be lost to a refresh that was in flight, which is the wrong direction for a security control.

A second window exists inside the gateway itself. `/authorize` and `/token` can mint a code or token pair for a client that `clients remove` deletes between the "is this client registered" check and the write.

## Decision

- **One advisory lock for the whole store.** Every read-modify-write holds an exclusive lock on `~/.onebrain/gateway/auth.lock` (created `0600`), taken with the standard library's `File::lock`. No new dependency.
- **Lock only the inner bodies.** The lock is not re-entrant (a second handle on the lock file conflicts even in the same process), so only the inner `issue_token_pair_for_resource` and `rotate_refresh_for_client` bodies lock, and the wrapper functions do not. A locked method never calls another locked public method; it uses the private load / save helpers.
- **Readers do not lock.** Writers replace files by atomic temp-file-and-rename, so a reader always sees one whole file.
- **Cap and insert in one critical section.** `register_client_capped` makes the 50-client check and the insert one locked operation, so `clients remove` and `/register` in different processes cannot race the cap.
- **Post-mint re-check.** After minting, `/token` and `/authorize` check that the client is still registered. If it was removed in between, the new token family is revoked (or the code burned). A removal after the re-check sees the new pair and revokes it, and a removal before it means the re-check fails, so there is no ordering that leaves a usable credential for a removed client.
- **`rust-version = "1.89"`** in the workspace, inherited by every crate. `File::lock` was stabilised in Rust 1.89, so the lock is what sets the minimum.

## Consequences

- A revocation made from any terminal takes effect on the client's next request, whether or not `gateway run` is up.
- Raising MSRV to 1.89 is a user-visible build requirement for anyone building from source; release binaries are unaffected.
- The lock has no timeout, so a stuck holder blocks other writers; `auth.lock` must not be deleted while the gateway runs.
- There is no `fsync` before the rename. A crash can lose the latest write, which for revocation means it must be repeated.
- Accepted gaps: `tokens revoke --client` leaves pending authorization codes (10 min TTL; `clients remove` deletes them), and the replay-path family revoke is best effort.

## Alternatives considered

- **A third-party file-lock crate** (for example `fs2`) (considered while writing this ADR). Rejected: the standard library now provides it.
- **Route CLI changes through the running gateway over HTTP.** Rejected: the commands must work with the gateway stopped.
- **A single process-wide mutex.** Does not cover the CLI process.
- **Move the store to SQLite or redb.** Heavier than four small JSON files, and a larger change than the bug warranted.
