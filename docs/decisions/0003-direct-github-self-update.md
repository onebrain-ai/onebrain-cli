# 0003 — Direct-GitHub-Release self-update (not npm/bun)

- **Status:** accepted
- **Date:** 2026-05-20

## Context

Through alpha.1–alpha.8 the self-update path shelled out to `bun install -g @onebrain-ai/cli@<v>` (Unix) / `npm install -g …` (Windows). But the v3.x Rust binary was never published to npm — so *every* real-world `onebrain update` failed with "package exists but version not found." Self-update was broken for the entire early alpha cycle, and the mechanism coupled a native binary's update to a package-manager that didn't carry it.

## Decision

Fetch the binary directly from GitHub Releases and swap it in place, with no package-manager middleware. Implemented in `onebrain-fs/src/update/install.rs`:

1. Resolve the running target triple at compile time via `cfg!` (`AssetInfo::for_running_target`).
2. Download `onebrain-<triple>.<ext>` from `releases/download/<tag>/` over HTTPS (rustls TLS).
3. Extract the binary from the tarball.
4. **Atomically** replace the running executable: write a sibling `.new`, `fsync`, `chmod 0755`, then `rename` over the target (Unix single-rename; Windows two-step `live→.old`, `.new→live`, with rollback if the second step fails).

## Consequences

- **Self-update actually works**, on the same per-platform binary that brew, the npm wrapper, and direct download all resolve to.
- **Trust boundary = GitHub's TLS chain** (rustls, no opt-out). At GA there is no SHA-256 or signature check of the asset itself — this matches the rustup/deno/bun baseline. SHA-256 verification is tracked for [v3.1.4](../../README.md#roadmap).
- **Windows zip extraction was intentionally stubbed at v3.0.0** — `update --plan` omits Windows triples so a Windows user isn't told a target is auto-installable and then hits an error.
- **Homebrew caveat:** because the swap rewrites `current_exe` in place, on a brew-managed install it diverges the Cellar binary from what brew recorded. Until brew-aware delegation lands (tracked v3.1.4), prefer `brew upgrade onebrain` on brew machines.
