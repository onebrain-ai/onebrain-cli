# Test Coverage

The CLI workspace targets **100% line coverage on testable "core" code** — command
handlers, parsers, pure helpers, and dispatch — while explicitly excluding code that
cannot be meaningfully exercised in tests (network installs, blocking servers, TTY
wizards, OS-specific probes).

Measure with:

```sh
scripts/coverage.sh          # summary, with the documented exclusions applied
scripts/coverage.sh --html   # detailed HTML report under target/llvm-cov/html
```

(`scripts/coverage.sh` wraps `cargo llvm-cov --workspace` with the `--ignore-filename-regex`
below. A clean run is required — `--no-clean -p <pkg>` reuses non-instrumented artifacts
and silently drops the binary crate from the report.)

## Excluded files (documented allowlist)

These are excluded from the coverage target because exercising them in a unit/integration
test would require mocking the network, spawning real subprocesses, running a blocking HTTP
server, or driving a TTY — out of proportion to the value. Each is listed with its reason so
the exclusion is an explicit decision, not a silent skip.

| File | Reason |
|---|---|
| `crates/onebrain-cli/src/main.rs` | Process bootstrap, panic-hook install, `std::process::exit` |
| `crates/onebrain-cli/src/commands/serve.rs` | Foreground blocking HTTP listener — never returns in a test |
| `crates/onebrain-cli/src/commands/daemon.rs` | Process fork/detach + PID files |
| `crates/onebrain-cli/src/commands/update.rs` | GitHub releases API + binary self-replace (network) |
| `crates/onebrain-cli/src/commands/qmd_reindex.rs` | Shells out to the `qmd` binary |
| `crates/onebrain-cli/src/commands/harness_run.rs` | Spawns `claude` / `gemini` |
| `crates/onebrain-cli/src/server/chat.rs` | SSE streaming proxy to a live AI subprocess |
| `crates/onebrain-cli/src/server/search.rs` | Live qmd-backed search path |
| `crates/onebrain-fs/src/update/install.rs` | npm/bun/brew install subprocess + network |
| `crates/onebrain-fs/src/init/wizard.rs` | Interactive TTY prompts |
| `crates/onebrain-fs/src/vault_sync/progress.rs` | Terminal progress-bar rendering |
| `crates/onebrain-cli/src/output/progress.rs` | Terminal progress-bar rendering |
| `crates/onebrain-cache/src/session_token.rs` | Platform-specific PPID / terminal-session probes |

Partial exclusions (a single network/subprocess function inside an otherwise-core file) use
an inline `// coverage:ignore-start` / `// coverage:ignore-end` region marker, documented at
the call site.

## Status (2026-06-30)

- Whole-workspace baseline (no exclusions): **89.58% line** (`cargo llvm-cov --workspace`).
- **Core (this initiative's target surface, exclusions applied): 94.28% line** / 94.63% region —
  `scripts/coverage.sh`. ~1,429 missed lines remain on core code (down from 1,711 baseline).

Closed so far:
- Phase 1 — `v31/dispatch.rs` 76.94% → 86.70% (stub/verb exit-code tests).
- Phase 2 — `commands/doctor.rs` 87.55% → 94.20%, `commands/register_schedule.rs` 72.08% →
  91.30%, `vault_ctx.rs` 51.35% → 100%, `commands/run_skill.rs` 78.82% → 79.17%.
- Phase 3 (fs cluster, +94 tests) — `note/archive.rs` 80.25% → 94.20%, `note/move.rs` → 95.49%,
  `init/mod.rs` 89.11% → 94.14%, `init/marketplace.rs` → 90.62%, `vault_sync/pin.rs` 93.43% → 97.16%,
  `vault_sync/orchestrate.rs` → 93.61%, `vault_sync/sync.rs` → 98.13%, `register_hooks/settings.rs`
  78.26% → 85.71%, `register_hooks/hooks.rs` → 97.83%, `migrate.rs` → 95.34%, `doctor/vault_yml_keys.rs`
  → 97.45%, `v31/hook_rewriter.rs` → 97.98%, `output/dispatcher.rs` → 95.60%. Every target improved;
  none reached 100% — residuals are hard error/`EXDEV`/edge paths and defensive test-guard arms.

A ratcheting CI coverage gate (fail under the current core %) will be added in the final phase,
once the core is at or near 100%, so it doesn't fight a moving target while the remaining phases land.

Remaining core gaps to close to reach the target (tracked, by missed lines): the fs-cluster
residuals not yet at 100% (`register_hooks/settings.rs` ~86%, `init/{mod,marketplace}.rs` ~94/91%,
`vault_sync/orchestrate.rs` ~94%, `note/move.rs` ~95%), the long tail of 1–6-missed-line files
(`note/{folder,walker,delete,stat,write}.rs`, `init/{safety,folders}.rs`, `doctor/*`, `backup.rs`),
`server/api.rs` (~69%, 265 missed — not yet classified target-vs-exclude), and the residual
interactive-spinner paths in `commands/run_skill.rs` (needs a pty harness — likely promoted to the
exclusion list). See `01-projects/onebrain/cli/2026-06-29-cli-coverage-100-design.md` for the phased plan.
