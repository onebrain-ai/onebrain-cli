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
| [0011](0011-release-opt-level.md) | Release build stays `opt-level = "z"` (size over speed) | accepted |
| [0012](0012-native-search-replace-qmd.md) | Native Rust search engine (replace external qmd) | accepted |
| [0013](0013-retrieval-semantics-confidence-gating.md) | Retrieval semantics: instruction prefixes, confidence floor, exact-word lex | accepted |
| [0014](0014-index-scope-and-exclusion.md) | Index scope: Markdown-only, built-in skips, `search.exclude` | accepted |
| [0015](0015-grouped-output-convention.md) | Grouped text-output convention (emoji sections, no frames) | accepted |
| [0016](0016-qmd-collection-config-migration.md) | `qmd_collection` → `search.collection` via `doctor --fix` | accepted |
| [0017](0017-platform-tiered-semantic-search.md) | Platform-tiered semantic search (`semantic` feature; lex-only fallback) | accepted |
| [0018](0018-release-build-strategy-lessons.md) | Release build strategy: win-arm64 cross-compile + pre-tag matrix rule | accepted |
| [0019](0019-native-mcp-server-staged-qmd-cutover.md) | Native MCP server (`onebrain mcp`) + staged qmd cutover | accepted |
| [0020](0020-cpu-only-embedding-runtime.md) | CPU-only embedding runtime, by packaging choice | accepted |
| [0021](0021-search-state-persistent-data-dir.md) | Native-search state moves to the persistent data dir | accepted |
| [0022](0022-honest-search-lock-errors.md) | Honest search lock & status errors (`E_ENGINE_BUSY`, exit 77) | accepted |
| [0023](0023-warm-daemon-mcp-search.md) | Warm daemon owns the search engine for mcp + CLI search | accepted |
| [0024](0024-vector-confidence-recall-first.md) | Recall-first vector cutoff + honest confidence | accepted · superseded in part by [0025](0025-tier2-cross-encoder-reranker.md) |
| [0025](0025-tier2-cross-encoder-reranker.md) | Tier-2 cross-encoder reranker on every search surface | accepted |
| [0026](0026-config-self-documentation.md) | Self-documenting onebrain.yml + doctor validate/reset-to-default | accepted |
| [0027](0027-collection-cache-layout-split.md) | Collection cache layout split (`index/` + `models/`) with eager migration | accepted |

> These ADRs distill the public-facing rationale; the full design notes live in the project tracker. Numbers are stable IDs assigned at authoring time — see each ADR's **Date** for chronology.
