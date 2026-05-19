---
latest_version: 3.0.0-alpha.0
released: 2026-05-19
---

# OneBrain CLI Changelog (v3.x · Rust)

All notable changes to the OneBrain CLI binary (`onebrain`) in the v3.x Rust rewrite.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

> **Versioning:** CLI version is tracked in workspace `Cargo.toml`. v3.x is the Rust port of [v2.x (TypeScript/Bun)](https://github.com/onebrain-ai/onebrain). First user-facing alpha is `v3.0.0-alpha.1` (planned 2026-06-02 per spec §7.1) — `v3.0.0-alpha.0` is an internal milestone marker without binary artifacts.

## [Unreleased]

- `register-hooks` subcommand · idempotent `.claude/settings.json` wiring · canonical exec-form Stop hook + conditional PostToolUse `onebrain qmd-reindex` (when `qmd_collection` set in vault.yml) + 14 OneBrain permission entries · legacy shell-form + `checkpoint-hook.sh` + `qmd update …` migration in place · stale-event cleanup (any onebrain-* command under events other than Stop / PostToolUse) · serde_json `preserve_order` so unknown top-level/nested keys survive round-trip in insertion order · flags: `--vault` (rename of Bun's `--vault-dir`) · `--dry-run` (new) · `--remove` (new uninstall path) · 68 unit + 7 Layer 2 integration + 2 Layer 3 snapshot + 3 Layer 4 parity scaffold tests
- `update` subcommand · GitHub releases/latest fetch via `reqwest::blocking` (rustls-tls, no openssl) + atomic install/validate gate · 4 injectable IO closures (fetch / install / validate / current-version) mirror the Bun `runUpdate` API · 20 unit + 6 mockito-backed integration + 2 insta snapshot + 1 parity scaffold · `--check` dry-run flag · `ONEBRAIN_GITHUB_RELEASES_URL` env override for tests · `reqwest 0.12` + `mockito 1` added to workspace deps per spec §2.6 (PR TBD)
- `run-skill` subcommand spawns `claude -p "<prompt>" --add-dir <vault>` with the vault as `cwd` and inherited stdio + env (so PATH/HOME survive for Homebrew lookups) · prompt builder namespaces bare names under `onebrain:<skill>` and preserves explicit plugin namespaces · `--arg key=value` repeated, insertion order preserved · `CLAUDE_BIN` env → `$HOME/.local/bin/claude` → `/opt/homebrew/bin/claude` → `/usr/local/bin/claude` → bare `claude` probe order with stderr warning when `CLAUDE_BIN` points to a missing path · exit codes: 78 missing vault.yml · 127 spawn error · 128+sig signal termination (Unix) · child code otherwise · 16 unit + 6 inline + 10 Layer 2 integration (mock claude bash script · no real claude needed in PATH) + 1 Layer 3 argv snapshot + 1 Layer 4 parity scaffold (PR TBD)
- `migrate <name> [--cutoff YYYY-MM-DD] [--vault DIR]` subcommand with idempotent `backfill-recapped` migration (walks `[logs]/session/YYYY/MM/*.md`, adds UTC `recapped:` to session-log frontmatter, preserves insertion order, inclusive ISO cutoff, EACCES/malformed → `skipped++` + stderr) · 21 unit + 6 Layer 2 + 1 Layer 3 snapshot + 2 Layer 4 parity scaffold (PR TBD)
- `doctor` subcommand with 8 read-only health checks behind `Box<dyn Check>` trait object: vault.yml · vault.yml-keys (required/soft/deprecated schema) · folders (8 PARA dirs) · plugin-files (.claude/plugins/onebrain integrity + stale .sh detection) · settings-hooks (Stop + PostToolUse exec/legacy/absent form + Bash(onebrain *) permission) · orphan-checkpoints · qmd-embeddings (3s timeout, non-fatal) · claude-settings (stale marketplace repo) · 41 unit tests + 7 Layer 2 integration + 1 Layer 3 snapshot + 1 Layer 4 parity scaffold (PR TBD)
- `VaultFolders` extended from 1 (`logs`) to all 8 standard keys (inbox · projects · areas · knowledge · resources · agent · archive · logs) with per-key serde defaults matching Bun `DEFAULT_FOLDERS` (PR TBD)
- `--fix` auto-repair deferred to v3.0.1 patch per spec §7.10 slip-handling — flag is parsed but emits a stub stderr message; doctor must be parity-green before GA but fix logic can ship in patch
- `orphan-scan` subcommand with Active-Session Guard (mtime-driven cross-harness live-session detection) and manual session log skip · `CheckpointPolicy { minutes: u32 }` field on `VaultConfig` drives the `max(60min, 2 * cp.minutes)` guard threshold · 38 unit tests + 3 Layer 2 integration + 1 Layer 3 snapshot + 2 Layer 4 parity (PR #3)
- New `onebrain-fs::orphan` module composes 5 internal helpers (`parse_checkpoint_filename`, `parse_frontmatter`, `has_manual_session_log`, `get_newest_mtime_ms`, `is_group_active_or_ambiguous`) with fail-safe propagation: any I/O ambiguity → group skipped rather than counted (Bun symmetry with `/wrapup`) (PR #3)
- `onebrain-core::load_vault_config_at(&Path)` helper for direct-path vault.yml loading without the `VaultRoot` invariant · used by Active-Session Guard threshold derivation (PR #3)
- `.github/workflows/release.yml` 7-platform release pipeline (darwin-{arm64,x64} · linux-{arm64,x64,musl-x64} · win-{x64,arm64}) · tar.gz / zip + sha256 · auto-detects prerelease from `-alpha`/`-beta`/`-rc` tag suffix · user-controlled inputs route through `env:` vars (PR #2)
- README clarifies `onebrain-cli` is the crate name; the produced binary is `onebrain` per `[[bin]]` in `crates/onebrain-cli/Cargo.toml` (PR #2)
- Post-merge fix-ups on PR #3: differentiate ENOENT from EACCES/EIO when reading vault.yml (silent vs stderr warning) · `frontmatter` module made `pub(crate)` to prevent visibility leak · scattered imports consolidated to top of `orphan.rs` · boundary tests added (`age == guard` counted · `minutes: 0` falls back to floor) · `.gitkeep` so empty parity fixtures survive git clone
- `CHANGELOG.md` reformatted to onebrain repo's compact style — frontmatter (`latest_version`, `released`) · conventional-commit-style per-version titles · flat detailed bullets ≤ 8 per version (PR #5 reformats PR #4's initial)
- GitHub repo metadata: description set · homepage `https://onebrain.run` · topics (rust, cli, obsidian, onebrain, ai-agent, claude-code) · main branch ruleset (5 required checks · squash-only · linear history · resolve threads · dismiss stale reviews)

## v3.0.0-alpha.0 — feat(slice-1): session-init + 4-crate workspace foundation

- 4-crate Cargo workspace: `onebrain-core` (types/config/path) · `onebrain-fs` (vault walks) · `onebrain-cache` (session token, qmd status) · `onebrain-cli` (binary · clap dispatch with all 13 subcommands scaffolded · 12 still `todo!()`) · workspace inheritance via `*.workspace = true` discipline · `publish = false` workspace-wide
- `session-init` subcommand with 8-layer session token resolution (Bun v2.3.3 parity): WT_SESSION → TMUX_PANE → TERM_SESSION_ID env vars (stripped + truncated to 8 chars) → `findClaudeAncestorPid` walk-up via `ps -o ppid=,comm=` (12-hop cap · Unix only) → `$TMPDIR/onebrain-day-YYYYMMDD.token` day-scoped cache → process ppid → PowerShell parent PID (Windows stub) → 5-digit numeric random fallback
- `qmd_unembedded` count sourced from spawning `qmd status --json` (matches Bun) instead of the originally-specced filesystem-walk approach · 2-second timeout · returns 0 on any failure · caught during PR #1 fix-up after 3-round review found 7 behavioral divergences from Bun
- Block path: BOTH `find_vault_root` returning `None` AND `load_vault_config` returning `Err` emit `{"decision":"block","reason":"onebrain-init-required"}` · session-init never exits non-zero (matches Bun contract for the Claude Code SessionStart hook)
- 4-layer test pyramid: inline unit + `assert_cmd` integration + `insta` snapshots + golden-master parity vs Bun v2.3.3 (verified byte-identical locally with `BUN_BINARY=~/projects/onebrain/dist/onebrain` · CI parity job fails until v2.3.3 release artifact is uploaded upstream)
- Error model split: `thiserror` typed errors per library crate (`CoreError` / `FsError` / `CacheError`) + `anyhow` propagation in binary with `.context()` chains · `classify_exit_code` walks `anyhow::chain()` to extract wrapped `CoreError` variants for sysexits.h-aligned exit codes (64/65/66/67)
- CI workflow: fmt + clippy + 3-platform test matrix (ubuntu/macos/windows) · `concurrency` block cancels outdated PR runs · `permissions: contents: read` hardening
- AGPL-3.0-only license · Windows ARM64 added to release matrix as the 7th platform per 2026-05-19 decision · forward-compat `tokio` scaffold (`tokio_helper::run_async` with `#[allow(dead_code)]`) ready for v3.1 server mode without restructuring main.rs · 46 tests passing

[Unreleased]: https://github.com/onebrain-ai/onebrain-cli/compare/v3.0.0-alpha.0...HEAD
[v3.0.0-alpha.0]: https://github.com/onebrain-ai/onebrain-cli/releases/tag/v3.0.0-alpha.0
