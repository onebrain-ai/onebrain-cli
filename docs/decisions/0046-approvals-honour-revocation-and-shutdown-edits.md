# 0046 — Waiting approvals honour revocation; shutdown lets the Telegram edit land

- **Status:** accepted
- **Date:** 2026-10
- **Follows:** [0038](0038-gateway-policy-engine-and-human-approvals.md), [0039](0039-gateway-approval-keepalive-and-wait-clamp.md) (revisits both), [0044](0044-gateway-auth-store-bounded-lock-and-atomic-exchange.md).
- **PRs:** [#441](https://github.com/onebrain-ai/onebrain-cli/pull/441); issues #427, #430.

## Context

**Revocation did not reach a call that was already waiting.** The bearer check runs once, when a request arrives. A tool call that then waits up to 270 s for a human kept running on a credential the operator had since revoked with `tokens revoke`, `tokens revoke --client` or `clients remove`. If the human then clicked Allow, the call wrote. Worse, the `/approvals` route recorded an `ask_once` grant at click time, so a revoked call could leave standing consent: after a single-token `tokens revoke <id>` the token family lives on through a refresh, and that grant would auto-allow the client's next call. A security review found this.

The CLI is a separate process that only edits the auth store files, so any fix must work with no message from the CLI to the gateway.

**Shutdown cut the Telegram edit off.** On SIGTERM the gateway denies every pending approval, and each denial ends in an `editMessageText` ("Gateway stopped") that runs on the blocking pool. The runtime teardown could cancel a slow edit and leave live Allow/Deny buttons pointing at a stopped gateway.

## Decision

- **Poll the store, no IPC.** `await_approval` runs a `revocation_watch` beside the wait. Every 5 s (`REVOCATION_CHECK_INTERVAL`) it re-reads the auth store through the read API (`check_access_state`, read-only, no `auth.lock`, on `spawn_blocking`). The first check is one interval in. When it finds the credential revoked it denies the approval through the same first-response-wins `Approvals::resolve` every channel uses, which also withdraws the native prompt.
- **What "revoked" means.** The presented token's status in `tokens list` is `Revoked`, the token is no longer in the store, or its client is no longer registered. An `Expired` token is not revoked: an access token lives 1 h and a wait can last 270 s, so expiry mid-wait is not withdrawn consent.
- **Allow-time check, fail closed.** An Allow is re-checked once more before anything is written. Revoked gives a denial with channel `revoked`. A store that cannot be read gives a denial with channel `unverified`. The periodic watch fails open on a read error (it keeps waiting and logs a warning), because the Allow-time check is what guards the write.
- **The principal carries the family and token id.** `Principal` gets `family` and `token_id` (the same id `tokens list` shows) so the check can find the exact token. Neither is serialized: not on `/approvals`, not in the audit log. The `/approvals` wire format is unchanged.
- **Grants are per token family.** `GrantKey` becomes `(client_id, family, vault, class)`. A family is stable across refresh rotation, so ordinary refreshes keep the grant, but a client that is revoked and consents again gets a new family and starts with no grants.
- **One place records grants.** The waiter's Allow path, after the Allow-time check passes, records the grant for every channel (HTTP, native, Telegram), and nothing is recorded under `ask_always`. The `/approvals` route no longer records a grant at click time, because a click can land after the credential was revoked.
- **New audit channels.** `revoked` and `unverified` join `native`, `http`, `telegram`, `shutdown` and `disconnect`. The client sees "access was revoked while this call waited for approval" or "could not verify access when this call was approved; nothing was written". A Telegram prompt is edited to "Access was revoked" or "Could not verify access".
- **Shutdown drain (#430).** `TelegramChannel` counts each `fire` and each outcome edit in an in-flight tracker (`EditTracker`/`EditGuard`; the last guard to drop wakes the waiter). `drain_edits` returns when nothing is in flight and no unedited live prompt remains. A live prompt counts because `deny_all` only wakes the waiters, and one may not have reached `note_outcome` yet. `gateway run` calls it after `deny_all`, with a budget of 2 s (`TELEGRAM_EDIT_DRAIN`) counted from the shutdown signal, and logs a warning if edits are still pending.

## Consequences

- A revoked credential ends its waiting calls within about 5 s, and an Allow after a revoke writes nothing and leaves no grant.
- The cost is one read of the auth files per waiting call every 5 s.
- A single-token `tokens revoke <id>` stays weak by design: it ends that token only, and the client can refresh on the same family. A full cut-off is `tokens revoke --client` or `clients remove`.
- An Allow can be denied as `unverified` when the store is unreadable (for example a corrupt file), even though the human approved. That is the intended fail-closed trade.
- Grants no longer survive a re-consent, so a client re-paired after a revoke is asked again. This is how #427's "drop that client's grants at revoke" is delivered: the CLI cannot reach the gateway's in-memory grants, and re-consent mints a new family, so an old grant can never match.
- Residual risks, stated plainly: (a) a revoke that lands in the milliseconds between the Allow-time check and the write can still let that one write through; (b) on a revoke race Telegram may briefly toast ✅ before the message is edited to "Access was revoked"; (c) grants for dead token families stay in memory until restart, though they never match again.
- The shutdown drain adds up to 2 s before exit, inside the existing grace period. An edit can still be lost if Telegram itself is unreachable.
- The check is polling, so there is a window of up to 5 s in which a revoked call is still waiting; the Allow-time check closes it for writes.

## Alternatives considered

- **Signal or IPC from the CLI to the gateway.** Rejected: the CLI must work with the gateway stopped, and a gateway that is up would need a new authenticated control channel.
- **Drop the client's grants when the CLI revokes.** Rejected: grants live in the gateway's memory and the CLI cannot reach them. Family scoping gives the same result without a message.
- **Check only at Allow time.** Rejected: a revoked call would sit pending until the human answered, and the prompt would stay on screen.
- **Fail the Allow-time check open.** Rejected: an unverifiable Allow must not write.
- **Wait longer or retry the Telegram edit at shutdown.** Rejected: the 2 s bound keeps shutdown within the supervisor's limit.
