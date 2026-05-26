# Architecture Decision Records

Each ADR captures one decision: the context that forced it, the choice made, and the consequences (good and bad). They're a paper trail — read them to understand *why* the code looks the way it does, not *what* it does (the source covers that).

## Format

Short and honest. One file per decision, numbered in rough chronological order:

```markdown
# NNNN — Title

- **Status:** accepted | superseded by [NNNN] | revisited
- **Date:** YYYY-MM-DD

## Context
What problem / constraint forced a decision.

## Decision
What we chose, stated plainly.

## Consequences
What this buys us, and what it costs (the tradeoff is the point).
```

ADRs are immutable once accepted — if a decision changes, write a new ADR that supersedes the old one rather than editing history.

## Index

| # | Decision | Status |
|---|---|---|
| [0001](0001-rust-rewrite.md) | Rust rewrite of the Bun/TS CLI | accepted |
| [0002](0002-four-crate-workspace.md) | Four-crate workspace split | accepted |
| [0003](0003-direct-github-self-update.md) | Direct-GitHub-Release self-update (not npm/bun) | accepted · revisited by [0008](0008-self-update-hardening.md) |
| [0004](0004-canonical-envelope.md) | Canonical `Envelope<T>` output shape | accepted |
| [0005](0005-text-default-output.md) | Text-default output · hooks inject `--json` | accepted |
| [0006](0006-locked-command-tree.md) | Locked `<noun> <verb>` command tree | accepted |
| [0007](0007-config-rename.md) | Config rename `vault.yml → onebrain.yml` | accepted |
| [0008](0008-self-update-hardening.md) | Self-update hardening: SHA-256 + Homebrew-aware | accepted |
| [0009](0009-raspberry-pi-arm-matrix.md) | Full Raspberry Pi / ARM matrix (ARMv6-first) | accepted |
| [0010](0010-npm-supply-chain.md) | npm supply chain: Trusted Publishers + checksum postinstall | accepted |

> These ADRs distill the public-facing rationale; the full design notes live in the project tracker. Numbers are stable IDs assigned at authoring time — see each ADR's **Date** for chronology.
