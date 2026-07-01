#!/usr/bin/env bash
# Measure test coverage on the "core" (testable) workspace code.
#
# Wraps `cargo llvm-cov --workspace` with the documented exclusion allowlist
# (see docs/coverage.md). A CLEAN run is required: `--no-clean -p <pkg>` reuses
# non-instrumented artifacts and silently drops the binary crate from the report.
#
# Usage:
#   scripts/coverage.sh            # text summary (--summary-only)
#   scripts/coverage.sh --html     # full HTML report under target/llvm-cov/html
#   scripts/coverage.sh --lcov     # lcov file at target/coverage.lcov (for CI)
#   scripts/coverage.sh --ci-gate  # print the summary AND fail if core line % drops
#                                  #   below CORE_LINE_THRESHOLD (the CI ratchet gate)
set -euo pipefail

# Files excluded from the coverage target — keep in sync with docs/coverage.md.
# Each entry is unreachable in tests without mocking the network, spawning a real
# subprocess, running a blocking server, or driving a TTY.
IGNORE_REGEX='(src/main\.rs|commands/(serve|daemon|update|qmd_reindex|harness_run)\.rs|server/(chat|search)\.rs|update/install\.rs|init/wizard\.rs|(vault_sync|output)/progress\.rs|cache/src/session_token\.rs)'

# Ratchet gate: CI fails if core line coverage drops below this. Set conservatively
# below the achieved % (≈95.6% macOS / ≈95.5% Linux after the long-tail mop-up) to
# absorb platform/measurement jitter; RAISE this number as coverage climbs — never
# lower it. Raised 94 → 95 once the mop-up gave ~0.5% headroom. See docs/coverage.md.
CORE_LINE_THRESHOLD="${CORE_LINE_THRESHOLD:-95}"

mode="${1:---summary-only}"
case "$mode" in
  --html)
    exec cargo llvm-cov --workspace --ignore-filename-regex "$IGNORE_REGEX" --html
    ;;
  --lcov)
    exec cargo llvm-cov --workspace --ignore-filename-regex "$IGNORE_REGEX" \
      --lcov --output-path target/coverage.lcov
    ;;
  --ci-gate)
    # --fail-under-lines makes cargo-llvm-cov exit non-zero when the aggregate
    # line % is below the threshold; --summary-only still prints the per-file table.
    exec cargo llvm-cov --workspace --ignore-filename-regex "$IGNORE_REGEX" \
      --summary-only --fail-under-lines "$CORE_LINE_THRESHOLD"
    ;;
  --summary-only | "")
    exec cargo llvm-cov --workspace --ignore-filename-regex "$IGNORE_REGEX" --summary-only
    ;;
  *)
    echo "usage: scripts/coverage.sh [--summary-only|--html|--lcov|--ci-gate]" >&2
    exit 2
    ;;
esac
