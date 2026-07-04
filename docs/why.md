# Why OneBrain CLI

Point an AI agent at a vault and it improvises — a different pile of `grep` / `ls` / `find` / `sed` each time, behaving differently on each harness and re-derived every session: slow, token-hungry, non-portable, sometimes wrong. **OneBrain CLI replaces that improvisation with one deterministic binary.**

- **Same behavior on every harness & model** — Claude Code and Gemini CLI both run `onebrain <noun> <verb>` and get identical output; switch harness without re-testing how your vault gets touched.
- **Cross-platform, one command** — the *same* `onebrain <noun> <verb>` runs on macOS, Linux, and Windows (Apple Silicon & Intel, x86_64 & ARM down to a Pi Zero) and returns the *same* typed result on every OS. Write a hook or script once; it behaves identically everywhere — no per-platform shell quirks (`sed`/`find`/path-separator differences) to work around.
- **Yours to extend, no waiting** — add a capability the harness/LLM doesn't have yet and every agent can use it immediately; they only learn the command, not implement the feature.
- **No re-deriving solved workflows** — search, capture, consolidate, checkpoint live in the binary, so the agent calls one command instead of re-reasoning the recipe each session. Fewer tokens, no drift.
- **Deterministic & safe** — a typed command with a frozen `Envelope` can't half-finish or quietly differ like an ad-hoc `rm` / `sed` pipeline. Same input → same output, scriptable by hooks.
- **Fast** — the binary returns in under 50 ms, skipping the latency of several tool calls for what's already one operation.
- **Local-first** — your vault, your data, your AI memory; no cloud round-trip.
- **Trustworthy install** — self-update verifies the binary's SHA-256 before swapping.

See also: [`install.md`](install.md) for how the trust model is enforced, and [`architecture.md`](architecture.md) for how the binary is put together.
