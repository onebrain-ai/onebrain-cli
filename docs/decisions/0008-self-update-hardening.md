# 0008 — Self-update hardening: SHA-256 verification + Homebrew-aware update

- **Status:** accepted
- **Date:** 2026-05-26
- **Revisits:** [0003](0003-direct-github-self-update.md)

## Context

[0003](0003-direct-github-self-update.md) shipped self-update with the GitHub TLS chain as the only trust boundary — no checksum of the asset itself — and swapped the binary in place regardless of how it was installed. Two gaps surfaced in use:

1. **No integrity check.** TLS authenticates the transport, not the bytes. A corrupted or tampered archive (CDN glitch, a proxy presenting a valid cert) would install without complaint. The release pipeline already publishes a `.sha256` beside every asset and the npm wrapper already verified it — but the Rust self-update path did not.
2. **Homebrew divergence.** On a brew install, `current_exe` resolves through the symlink to `…/Cellar/onebrain/<ver>/bin/onebrain`, so the in-place swap rewrote the Cellar file directly — desyncing brew's metadata from disk and leaving a non-brew-managed file shadowing the symlink (observed firsthand).

## Decision

Harden the self-update path in v3.1.4:

- **Verify SHA-256 before the swap.** Download the published `<archive>.sha256`, compute the archive's digest, and compare — *before* extraction or swap. An unverifiable asset (missing/malformed `.sha256`) or a mismatch is a hard `UpdateError::Checksum`; the live binary is never touched. Fail-closed.
- **Detect the install channel and delegate.** A binary that canonicalizes under `…/Cellar/onebrain/…` is classified `Homebrew` and routed to `brew upgrade onebrain` instead of the in-place swap. Direct / manual installs keep the fetch-and-swap path.

## Consequences

- Self-update now refuses a binary it can't verify, closing the integrity gap left open at GA.
- Brew installs stay brew-managed — no more Cellar/symlink divergence; `onebrain update` "just works" on a brew machine.
- **Still an integrity check, not authenticity.** The `.sha256` is served from the same origin as the archive, so an attacker controlling that origin could serve a matching pair. Cosign/Sigstore signature verification remains a follow-up, pending release-side signing.
- Adds a `sha2` dependency and one `brew` subprocess on the Homebrew path.
