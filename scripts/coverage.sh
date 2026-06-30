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
set -euo pipefail

# Files excluded from the coverage target — keep in sync with docs/coverage.md.
# Each entry is unreachable in tests without mocking the network, spawning a real
# subprocess, running a blocking server, or driving a TTY.
IGNORE_REGEX='(src/main\.rs|commands/(serve|daemon|update|qmd_reindex|harness_run)\.rs|server/(chat|search)\.rs|update/install\.rs|init/wizard\.rs|(vault_sync|output)/progress\.rs|session_token\.rs)'

mode="${1:---summary-only}"
case "$mode" in
  --html)
    exec cargo llvm-cov --workspace --ignore-filename-regex "$IGNORE_REGEX" --html
    ;;
  --lcov)
    exec cargo llvm-cov --workspace --ignore-filename-regex "$IGNORE_REGEX" \
      --lcov --output-path target/coverage.lcov
    ;;
  --summary-only | "")
    exec cargo llvm-cov --workspace --ignore-filename-regex "$IGNORE_REGEX" --summary-only
    ;;
  *)
    echo "usage: scripts/coverage.sh [--summary-only|--html|--lcov]" >&2
    exit 2
    ;;
esac
