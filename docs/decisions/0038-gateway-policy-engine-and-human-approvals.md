# 0038 — Policy engine with risk tiers and human approvals

- **Status:** accepted · revisited by [0046](0046-approvals-honour-revocation-and-shutdown-edits.md) (grants are per token family and recorded only by the waiter; waiting calls honour revocation)
- **Date:** 2026-10
- **Follows:** [0036](0036-remote-mcp-gateway-separate-process.md), [0037](0037-gateway-oauth-authorization-server-and-pairing.md).
- **PRs:** [#408](https://github.com/onebrain-ai/onebrain-cli/pull/408) (policy, approvals, audit, `brain_capture`), [#409](https://github.com/onebrain-ai/onebrain-cli/pull/409) (Telegram channel), [#415](https://github.com/onebrain-ai/onebrain-cli/pull/415) (disconnect / shutdown denial).
- **Design:** "OneBrain Remote MCP Gateway — Design Spec", section on approvals and policy.

## Context

A valid OAuth token proves that a client was paired once; it does not prove that a particular write is wanted right now. A model-driven client can call a write tool at any time, and the person who must say yes is often away from the Mac (that is the point of the phone use case). The gateway needs a per-call decision that a human can take over, on whichever device they have.

## Decision

**Every tool call passes through a policy gate: classify, check the mode, optionally ask a human, then audit.**

- **Risk classes and modes.** Each tool is `read_only`, `mutating` or `destructive`. Each class has a mode in `gateway.yml`'s `policy:` block: `auto` (allow), `ask_once` (approval creates a grant), `ask_always` (approval every time, never satisfied by a grant) or `deny` (never offered). Defaults: `read_only: auto`, `mutating: ask_once`, `destructive: ask_always`, so an empty config never silently auto-allows a write.
- **Grants.** An `ask_once` approval records an in-memory grant keyed on `(client_id, vault, risk class)` for `policy.grant_ttl_minutes` (default 30). The vault is part of the key, because it is the consent the human was shown; the tool is not, because tools in a class are equally powerful. An `ask_always` approval records nothing. Grants are never persisted and vanish on restart.
- **Scope check first.** A token whose `scope` does not cover a tool's pack is denied before the mode is consulted.
- **Three channels, one registry.** A native macOS dialog (`osascript`), the loopback-only `GET/POST /approvals` page (pairing-code gated), and a dedicated Telegram bot with Allow / Deny buttons (`gateway telegram setup`). All resolve the same pending-approval registry and **the first answer wins**; the others become no-ops, and the Telegram message and macOS dialog are updated or withdrawn to match.
- **Fail closed.** A timeout is a denial and records no grant. A client that disconnects while its call waits is denied (audit channel `disconnect`), and a gateway shutdown denies everything pending (channel `shutdown`), so nothing is written for a caller who can no longer see the answer. The pending registry is bounded (16 overall, 4 per client).
- **Telegram authorization** is the tapping user's id equalling the configured private `chat_id`. It deliberately shares no state with the pairing-code lockout.
- **Audit log.** Every call, allowed or not, appends one redacted JSON line to `~/.onebrain/gateway/audit/YYYY-MM.jsonl`: decision (`auto`, `approved`, `denied`, `timedout`), channel, duration, outcome. A note body never appears, only its character count. A failed audit write never fails the call.
- **`capabilities` is truthful.** It reports each tool's class, the mode in force, and which approval channels can actually deliver a prompt on this machine now.

## Consequences

- Reads are cheap and writes are slow by design: the first `brain_capture` waits for a human.
- The audit log has no rotation yet; each line is bounded, the file count is not.
- Known gap: `notifications/cancelled` is ignored by rmcp in stateless mode, so cancellation is detected by transport close only; a client that cancels but keeps the stream open leaves its approval pending until the wait expires.
- A `tokens revoke` does not stop an approval that is already pending.
- Only `brain_capture` is mutating today; no tool is classified `destructive`, and the developer pack with shell and git tools is not shipped.

## Alternatives considered

- **Rely on the client's own confirmation UI.** Rejected as the only layer: it varies by client and cannot be audited by the gateway. It remains defence in depth.
- **A single allow / deny switch per tool.** Rejected: it cannot express "ask once, then trust for a while".
- **Persisted grants.** Rejected: consent for one running process should not become a standing credential on disk.
- **A single approval channel.** Rejected: the dialog does not help away from home, and Telegram alone fails when the phone is not at hand.
