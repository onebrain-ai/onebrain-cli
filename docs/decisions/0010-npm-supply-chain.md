# 0010 — npm wrapper supply chain: Trusted Publishers + checksum-verified postinstall

- **Status:** accepted
- **Date:** 2026-05-22

## Context

The npm wrapper (`@onebrain-ai/cli`) installs by downloading the GitHub Release binary in a `postinstall` step (the rustup / esbuild / swc pattern — see the [repo README](../../README.md#install) for why npm is a secondary channel). Two supply-chain risks come with that shape: (1) a long-lived `NPM_TOKEN` is a leak-and-rotation liability, and (2) the wrapper publish (npm) and the binary it fetches (GitHub) are separate artifacts — corruption or tampering between them would otherwise go unnoticed.

## Decision

- **Publish via npm Trusted Publishers (OIDC).** CI exchanges a short-lived OIDC token (`id-token: write`) for publish rights and ships `--provenance` for a Sigstore attestation — no stored secret. Manual local publishes are rejected by the trusted-publisher policy.
- **Verify the binary in postinstall.** Before extracting, `postinstall.js` downloads the asset's `.sha256` and checks it, then smoke-runs `onebrain --version` so a wrong-libc/arch download fails loudly at install time instead of on first use.

## Consequences

- No long-lived npm secret to rotate; every publish is attested to a specific workflow run + commit.
- The wrapper version always equals the binary release version (`npm version` is rewritten from the git tag in CI); prerelease tags (`-alpha`, `-rc`, …) skip the npm publish.
- A corrupted/tampered archive or a wrong-platform pick fails at install with an actionable error rather than silently shipping a broken binary.
- Pairs with the Rust self-update path's own SHA-256 check ([0008](0008-self-update-hardening.md)) — both install routes verify the same published checksum.
