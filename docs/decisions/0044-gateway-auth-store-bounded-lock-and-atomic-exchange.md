# 0044 — Gateway auth store: bounded fair lock wait, no in-process mutex, atomic code exchange, durable writes

- **Status:** accepted
- **Date:** 2026-10
- **Follows:** [0041](0041-gateway-auth-store-lock-and-toctou-recheck.md) (revisits it: the "no timeout" consequence and the post-mint re-check on `/token`).
- **PRs:** [#438](https://github.com/onebrain-ai/onebrain-cli/pull/438); issues #428, #429.

## Context

ADR 0041 put every read-modify-write of the gateway's four JSON files behind an exclusive lock on `~/.onebrain/gateway/auth.lock`, with no timeout. Two problems followed.

**A stuck holder stalled the whole gateway.** The gateway also kept its `AuthStore` behind a `Mutex<AuthStore>`, and handlers waited for `auth.lock` while holding that mutex, on async runtime workers. A CLI process suspended with Ctrl-Z while holding `auth.lock` therefore parked a worker per request, and every other handler queued behind the mutex, including the bearer-token check on `/mcp`. Long-lived tool calls (approval keep-alives) starved with them.

**Writes were not durable.** `write_json_atomic` renamed a temp file into place without `fsync`, so a power cut could lose a write the caller had been told succeeded.

Removing the mutex and bounding the wait exposed three further problems that the first version of the fix had to solve:

- The first bounded wait polled `try_lock` with sleeps. A waiter almost never landed in the microsecond gap between a busy writer's holds, so a write burst starved it into a spurious "busy" (it showed up as a flaky Windows CI test).
- The code grant used to be several separate lock holds (consume the code, mint, link the token family). With the mutex gone, a replay landing between "consume" and "link" found no family to revoke, so the first token pair survived.
- A failed directory `fsync` after the rename returned an error for a write that was already visible. For a refresh rotation that meant a 500, a client retry, and reuse detection burning the whole token family.

## Decision

- **Bounded wait.** `lock_exclusive` waits at most 5 seconds (`LOCK_WAIT`) and then fails with a typed `StoreBusy`. The CLI builds its message from it. `StoreBusy` is raised by every locked mutator.
- **No in-process mutex.** `AuthCtx.store` is a plain `AuthStore` shared across threads. Each `lock_exclusive` call opens its own handle on `auth.lock`, so the file lock serializes threads exactly as it serializes processes. Handlers run every store call that can wait on a `spawn_blocking` thread, never on a runtime worker. The bearer check and pairing-code verification (`verify_pairing`) take no lock.
- **503 on busy.** `/token` and `/register` answer `503 {"error":"temporarily_unavailable"}` with `Retry-After: 2`. `/authorize` answers a `503` HTML page with the same header. Nothing is spent: the client keeps its code or refresh token and retries.
- **The holder is named.** The process holding `auth.lock` writes `{pid, version}` to `~/.onebrain/gateway/auth.lock.holder` right after it takes the lock, and removes it before releasing. A `StoreBusy` reads that sidecar, so `gateway tokens revoke` and `gateway clients remove` can say "onebrain 3.5.1 (pid N) has held auth.lock for over 5s" and tell the user to wait for or resume that process. The sidecar is a separate file because a Windows whole-file lock can stop other handles reading the locked file.
- **Fair, kernel-queued wait.** The wait is a blocking `File::lock` on a helper thread, with the caller waiting on a channel with a deadline. The waiter is queued in the kernel and woken on release, so a write burst cannot starve it. The uncontended fast path is one `try_lock`, skipped while another thread of this process is already queued. A helper that outlives its deadline stays blocked until the lock is granted, then drops the file at once and so releases it. Helpers are capped at 64 in-process: beyond that the lock is treated as stuck and callers get `StoreBusy` immediately instead of spawning more threads.
- **Atomic code exchange.** `AuthStore::exchange_code` does the whole authorization-code redemption in one `auth.lock` hold: consume the code, check the PKCE/redirect bindings, check the client is still registered, mint the pair, link the family. The replay branch revokes the family in the same call. This also moves `/token`'s registration check inside the exchange, before minting, instead of after (ADR 0041 checked after mint). A busy lock spends nothing. `/authorize` keeps its post-mint re-check.
- **Durable writes.** `write_json_atomic` fsyncs the temp file before the rename and, on Unix, fsyncs the parent directory after it. A failure at any point before the rename removes the temp file and returns the error. Once the rename has succeeded the write is committed and visible, so a failed directory fsync only logs a warning: returning an error there would have made a refresh answer 500 and the client's retry burn the family. Windows has no directory fsync; there the rename's own durability is NTFS's.
- **`gateway run` fails fast.** Startup mints or reads the pairing code under the same lock, so if the lock stays held past 5 seconds the process exits with an error rather than serving. (Purging expired records at startup stays best-effort.)
- **CodeQL.** The `rust/path-injection` alerts on the new code are false positives and were dismissed: the axum `State` taint reaches a store root that is derived from `home_dir`, not from request data. Wrapping the calls differently did not change the analysis, so the code was not contorted to satisfy it.

## Consequences

- A stalled CLI can no longer freeze the gateway: affected requests get a retryable `503` within about 5 seconds, and `/mcp` bearer checks never wait on `auth.lock`.
- Writers in the same process and in other processes queue fairly, so a burst of writes does not turn into spurious `503`s.
- A busy lock now fails a whole operation (a code exchange, a refresh) rather than doing part of it.
- The holder sidecar is best-effort. A holder killed with SIGKILL leaves it behind, and if the next holder is a pre-3.5.1 or foreign process (which never rewrites it), a waiter can name a dead pid. There is no liveness check on the gateway side.
- A timed-out waiter thread stays blocked until the lock is granted. They are bounded at 64, but a lock that is stuck for a long time holds up to that many threads.
- Directory fsync adds a little latency to each write on Unix.
- The accepted gaps from ADR 0041 (`tokens revoke --client` leaves pending codes; the replay-path family revoke is best effort) are unchanged.

## Alternatives considered

- **Keep the mutex and add a timeout to the file lock.** Rejected: the mutex itself is what serialized unrelated handlers behind one stalled wait.
- **Poll `try_lock` with a shorter sleep.** Rejected: it still loses races against a busy writer and burns CPU; the kernel queue is the fair primitive.
- **Fail immediately on a busy lock (no wait).** Rejected: ordinary short contention between the gateway and a CLI command should just work.
- **Keep the split consume / mint / link flow and add a recheck.** Rejected: the replay race is inherent to separate holds; one hold removes it.
- **Return an error when the directory fsync fails.** Rejected for the burned-family reason above.
