# 0039 — Approval keep-alive over SSE, wait clamp (240 s / 270 s) and SIGTERM handling

- **Status:** accepted · revisited by [0046](0046-approvals-honour-revocation-and-shutdown-edits.md) (shutdown waits up to 2 s for Telegram edits)
- **Date:** 2026-10
- **Follows:** [0038](0038-gateway-policy-engine-and-human-approvals.md).
- **PRs:** [#415](https://github.com/onebrain-ai/onebrain-cli/pull/415); the service side is in [#419](https://github.com/onebrain-ai/onebrain-cli/pull/419).
- **Design:** "Gateway tunnel + service — Design" (decision D1).

## Context

With the policy engine, a gated tool call can wait for a human. The gateway ran `rmcp` with `with_json_response(true)`, which returns one JSON body at the end, so a call waiting on an approval sent **zero bytes** until it was resolved. Behind a Cloudflare tunnel that fails: Cloudflare cuts a response that has not started within its limit (as documented by Cloudflare at the time, 2026-10: 100 s idle and 125 s to first byte), and the original default wait was 300 s. The Claude connector also abandons a tool call at 300 s.

Evidence gathered before building (smoke test, 2026-10-09, real Claude connector through a quick tunnel):

- A spike showed that emitting one non-terminal notification right after the approval is registered makes `rmcp` open an SSE stream: first byte after 6 ms instead of 70 s, a `:` keep-alive comment every 15 s, and the result on the same stream. `json_response` stays `true` for every other call.
- Run 3 on home Wi-Fi (observed, 2026-10-09) held an approval for **240.8 s** with SSE keep-alives and the test passed: the client received the result, then continued on the same connection. So the connector accepts an SSE reply and ignores the notification and comments, Claude's tool timeout is above 240 s, and Cloudflare passes the stream.
- Run 1 and 2 on a phone hotspot failed for an unrelated reason: `cloudflared` over QUIC could not reach the edge, did not fall back to HTTP/2 for more than two minutes (tunnel 530), and later held a single edge connection that dropped mid-wait. Forcing `--protocol http2` fixed the connectivity ([0042](0042-gateway-tunnel-and-launchagent-service.md)). It was not a keep-alive defect.
- Run 2 also showed that after the client's request was cancelled the approval stayed approvable, and approving it still wrote the note although the client never got the result.

## Decision

- **SSE keep-alive for gated calls only.** Right after registering a pending approval, the gateway emits a notification (`notifications/progress` when the request carries a `progressToken`, otherwise `notifications/message`, which needs `enable_logging()` and a no-op `logging/setLevel`). `rmcp` then switches that reply to SSE with its 15 s keep-alive. Calls whose first message is also their last (reads, auto-allowed, granted, denied) stay plain JSON.
- **Wait clamp.** `policy.approval_wait_seconds` defaults to **240** and is clamped to at most **270**, with a startup warning when the configured value is higher, and a `doctor` check. The headroom below Claude's 300 s is for the final reply to travel. The earlier "10 minutes" in the design was corrected.
- **Disconnect denies.** If the transport closes while a call waits, the approval is denied (channel `disconnect`), prompts are withdrawn, and nothing is written; a `biased` select makes the disconnect win a race with an approve.
- **SIGTERM is a graceful shutdown.** On Unix `gateway run` treats SIGTERM like Ctrl-C, because launchd stops agents with SIGTERM. Handlers are registered before binding. Pending approvals are denied (channel `shutdown`), the native dialog is withdrawn, open requests get a 5 s grace, and runtime teardown is bounded to about 1 s, comfortably inside launchd's SIGKILL deadline.

## Consequences

- An approval-gated call survives a tunnel with an idle limit far below the wait.
- Keep-alive through a **named** Zero Trust tunnel was proven end to end at the phone check, not only the quick tunnel.
- The Telegram edit "Gateway stopped, denied" can be cut off on a slow network during the 1 s teardown, leaving an Allow button that does nothing.
- Cancellation by `notifications/cancelled` remains undetected ([0038](0038-gateway-policy-engine-and-human-approvals.md)).
- `approval_wait_seconds: 0` is legal and fails closed; it warns at startup because in a real config it is almost always a typo.

## Alternatives considered

- **Durable tasks that detach long work and are polled.** Planned in the original design and moved to the v3.6 Agent Runtime. The Brain tools are short; only the human wait is long, and keep-alive covers that.
- **Progress notifications only, keeping JSON replies.** Rejected: a JSON reply sends no bytes until the end.
- **A 600 s wait.** Rejected: above the client's 300 s tool timeout the call is abandoned anyway.
