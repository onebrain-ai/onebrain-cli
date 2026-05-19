# Changelog

All notable changes to OneBrain CLI v3.x are documented in this file.

Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) · this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `orphan-scan` subcommand with Active-Session Guard (mtime-driven live-session detection) and manual session log skip · full Bun v2.3.3 parity (PR #3)
- `CheckpointPolicy { minutes: u32 }` field on `VaultConfig` (default 30) driving the guard threshold (PR #3)
- `onebrain-core::load_vault_config_at(&Path)` helper for direct-path vault.yml loading without `VaultRoot` invariant (PR #3)
- `onebrain-fs::orphan` module: 5 helpers + `scan_orphans` entry · 38 unit tests · `OrphanScanResult` type (PR #3)
- `onebrain-fs::frontmatter::parse_frontmatter` · CRLF-aware YAML mapping extractor (crate-private · PR #3)
- `.github/workflows/release.yml` · 7-platform release pipeline (darwin-{arm64,x64} · linux-{arm64,x64,musl-x64} · win-{x64,arm64}) · auto-detects prerelease from tag suffix (PR #2)

### Changed

- README clarifies that the `onebrain-cli` crate produces the `onebrain` binary (PR #2)

## [v3.0.0-alpha.0] — 2026-05-19

First milestone marker · Slice 1 foundation. Pre-release tag; no public binary artifacts yet (next user-facing alpha will be `v3.0.0-alpha.1` per spec §7.1).

### Added

- 4-crate Cargo workspace: `onebrain-core` (types/config/path) · `onebrain-fs` (vault walks) · `onebrain-cache` (session token/qmd status) · `onebrain-cli` (binary)
- `session-init` subcommand: 8-layer session token resolution including `findClaudeAncestorPid` walk-up · 8-char env truncation · `$TMPDIR/onebrain-day-YYYYMMDD.token` day-scoped cache · 5-digit numeric random fallback · full Bun v2.3.3 parity
- `clap` derive dispatch with all 13 subcommands scaffolded · 12 still `todo!()`
- 4-layer test pyramid: inline unit + `assert_cmd` integration + `insta` snapshots + golden-master parity vs Bun v2.3.3
- CI workflow: fmt + clippy + 3-platform test matrix · `concurrency` block (cancel outdated runs) · `permissions: contents: read` hardening
- Error model split: `thiserror` typed errors per library crate + `anyhow` propagation in binary
- Forward-compat `tokio` scaffold (`tokio_helper::run_async`) ready for v3.1 server mode
- AGPL-3.0-only license · `publish = false` workspace-wide · 46 tests · byte-identical parity with Bun v2.3.3 locally

[Unreleased]: https://github.com/onebrain-ai/onebrain-cli/compare/v3.0.0-alpha.0...HEAD
[v3.0.0-alpha.0]: https://github.com/onebrain-ai/onebrain-cli/releases/tag/v3.0.0-alpha.0
