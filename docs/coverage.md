# Test Coverage

The CLI workspace targets **maximum testable line coverage on "core" code** — command
handlers, parsers, pure helpers, and dispatch — while explicitly excluding code that
cannot be meaningfully exercised in tests (network installs, blocking servers, TTY
wizards, OS-specific probes).

> **Realistic ceiling (verified 2026-06-30):** literal 100% is **not** reachable on the
> stable toolchain. cargo-llvm-cov has no inline `// coverage:ignore` support, and
> `#[coverage(off)]` is nightly-only — still gated behind the experimental
> `coverage_attribute` feature (rust-lang/rust#84605, open); on rustc 1.96.0 stable it errors
> `E0658`. (Even if stabilized, annotating a whole arm/closure would suppress the surrounding
> reachable code too — documenting residuals with reasons is the better policy regardless.)
> So genuinely-unreachable lines — defensive `match` arms, `spawn_blocking` panic-only
> `JoinError` closures, post-`canonicalize` TOCTOU I/O errors, body-limit-middleware dead
> branches, platform-defensive arms — can be neither covered nor ignored. The target is
> therefore **≈99% core + a documented residual list (below) + a ratcheting CI gate set at
> the achieved %**. (Decided over switching to nightly `#[coverage(off)]` or grcov, to keep
> the stable toolchain.)

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

Partial residuals (a few genuinely-unreachable lines inside an otherwise-core file) **cannot**
be inline-ignored on the stable toolchain (see the ceiling note above) — they are instead
listed in the **Residual unreachable lines** section below with a per-line reason, and the
file stays just under 100%. A whole file only joins the exclusion table above when *most* of
it is untestable.

### Residual unreachable lines (documented, keep file <100%)

`crates/onebrain-cli/src/server/api.rs` (~80 lines, after phase 3b):
- `spawn_blocking` `JoinError` closures (fire only if the blocking closure panics): tree walk, task scan, file/raw read, upload, post/put/delete write, move, folder create/delete.
- Post-`canonicalize` I/O errors (TOCTOU vanish / EACCES on stat or read of an already-resolved path): file stat/read, raw stat/read.
- FS write failure after pre-validation (`write`/`create_dir_all` fails post-check): upload.
- `get_config` non-missing/non-YAML config-load error arm.
- Body-limit middleware rejects before the handler (`DefaultBodyLimit::max(MAX_RAW_BYTES)`): upload over-cap branch is dead.
- Canonicalize / server-fault arms (vault root canonicalize fails though root existed at bind).
- Platform-defensive `RootDir`/`Prefix` path-component arms (unreachable on unix — caught by `is_absolute()`; platform-defensive on Windows, where a drive-relative `C:foo` carries a `Prefix` component yet `is_absolute()` is false).
- Defensive "can't happen" arms (WalkDir entry `Err`/`strip_prefix`, `HeaderValue::from_str` on a sanitized filename, ancestor-loop past canonical root) + llvm-cov closing-brace region artifacts.

## Status (2026-06-30)

- Whole-workspace baseline (no exclusions): **89.58% line** (`cargo llvm-cov --workspace`).
- **Core (this initiative's target surface, exclusions applied): 94.84% line** —
  `scripts/coverage.sh`. ~1,292 missed lines remain on core code (down from 1,711 baseline).

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
- Phase 3b (server/api.rs, +28 oneshot/unit tests) — `server/api.rs` 69.56% → 87.06%
  (covered no-vault 503s for all handlers, byte-range 206, forced-attachment, upload, move/folder
  conflict + 404/409, method 405, `If-Match` overwrite, error-mapping unit tests). The remaining
  ~80 lines are the documented **Residual unreachable lines** above (genuinely untestable on stable).

A ratcheting CI coverage gate (fail-under the achieved core %) lands in the final phase — once
the remaining testable gaps are closed and the residuals are documented, so it doesn't fight a
moving target. Literal 100% is not the bar (see the ceiling note up top); the gate locks in
whatever % the documented-residual set leaves.

Remaining core gaps to close to reach the target (tracked, by missed lines): the larger residuals
in `commands/doctor.rs` (~112) and `v31/dispatch.rs` (~102), `onebrain-fs/src/update/mod.rs` (~84),
`commands/register_schedule.rs` (~68), the fs-cluster residuals not yet maxed
(`register_hooks/settings.rs` ~86%, `init/{mod,marketplace}.rs` ~94/91%, `vault_sync/orchestrate.rs`
~94%), the long tail of 1–6-missed-line files (`note/{folder,walker,delete,stat,write}.rs`,
`init/{safety,folders}.rs`, `doctor/*`, `backup.rs`), and the residual interactive-spinner paths in
`commands/run_skill.rs` (needs a pty harness — likely promoted to the exclusion list). `server/api.rs`
is at its testable ceiling (87.06%; residuals documented above). See
`01-projects/onebrain/cli/2026-06-29-cli-coverage-100-design.md` for the phased plan.
