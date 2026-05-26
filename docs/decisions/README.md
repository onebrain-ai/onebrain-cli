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
| _0002_ | Four-crate workspace split | _planned_ |
| _0003_ | Direct-GitHub-Release self-update (not npm/bun) | _planned_ |
| _0004_ | Canonical `Envelope<T>` output shape | _planned_ |
| _0005_ | Text-default output · hooks inject `--json` | _planned_ |
| _0006_ | Locked `<noun> <verb>` command tree | _planned_ |
| _0007_ | Config rename `vault.yml → onebrain.yml` | _planned_ |

> Entries in _italics_ are stubs to be filled in. The full rationale for each currently lives in the project's design notes; these ADRs distill the public-facing version.
