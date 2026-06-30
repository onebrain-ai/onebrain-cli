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

## Status (2026-06-29)

- Whole-workspace baseline (no exclusions): **89.58% line** (`cargo llvm-cov --workspace`).
- **Core (this initiative's target surface, exclusions applied): 93.62% line** / 94.16% region —
  `scripts/coverage.sh`. ~1,517 missed lines remain on core code (down from 1,711 baseline).

Closed so far:
- Phase 1 — `v31/dispatch.rs` 76.94% → 86.70% (stub/verb exit-code tests).
- Phase 2 — `commands/doctor.rs` 87.55% → 94.20%, `commands/register_schedule.rs` 72.08% →
  91.30%, `vault_ctx.rs` 51.35% → 100%, `commands/run_skill.rs` 78.82% → 79.17%.

A ratcheting CI coverage gate (fail under the current core %) will be added in the final phase,
once the core is at or near 100%, so it doesn't fight a moving target while the remaining phases land.

Remaining core gaps to close to reach the target (tracked, in priority order by missed lines):
the `onebrain-fs` 90–96% cluster (`note/archive.rs`, `note/move.rs`, `migrate.rs`,
`register_hooks/*`, `init/{mod,marketplace}.rs`, `doctor/vault_yml_keys.rs`, `vault_sync/*`
non-progress, `v31/hook_rewriter.rs`), plus the residual interactive-spinner paths in
`commands/run_skill.rs` (needs a pty harness — likely promoted to the exclusion list). See
`01-projects/onebrain/cli/2026-06-29-cli-coverage-100-design.md` in the vault for the phased plan.
