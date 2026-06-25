# 0001 — Rust rewrite of the Bun/TS CLI

- **Status:** accepted
- **Date:** 2026-05-14

## Context

The v2.x CLI was a TypeScript binary compiled with Bun. It worked, but it carried real costs for a tool that runs on every session-start hook and every checkpoint:

- ~57.8 MB stripped binary and ~21 MB private memory per invocation — heavy for a process spawned dozens of times a day.
- Cold start measured in the hundreds of milliseconds, which the user *feels* on every hook.
- A Bun runtime dependency baked into the distribution story.

The CLI is also meant to become the foundation other surfaces build on (Studio, scheduled skills, a future daemon). A scripting-runtime binary is an awkward base for that.

## Decision

Rewrite the CLI in Rust as a single static binary, in a new repo (`onebrain-ai/onebrain-cli`) separate from the plugin (`onebrain-ai/onebrain`). Port the v2.x command surface 1:1 first (parity), then evolve. Keep the vault format, config schema, plugin contract, and slash-command surface unchanged so existing vaults keep working.

## Consequences

- **~10× less memory, ~92% smaller binary, sub-50 ms cold start** — the per-hook cost the user feels drops to near-zero.
- **Single static executable per platform** — no runtime to install; distribution is just "download the binary" (see [0003](0003-direct-github-self-update.md)).
- **A real systems-language base** for the daemon/RPC/mesh work on the roadmap.
- **Cost:** a full reimplementation, and a golden-master parity suite against the Bun binary to prove behavior matched (since retired in v3.1.0 once the contract was pinned by the `Envelope` snapshots).
- Licensed MIT OR Apache-2.0; the workspace is `publish = false` (see [0002](0002-four-crate-workspace.md)).
