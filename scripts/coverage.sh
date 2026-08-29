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
#
# HIGH-ASSURANCE POLICY (repo owner): security/filesystem-touching code is NOT
# whole-file-excluded. `daemon.rs`, `daemon_client.rs`, and `server/search.rs`
# are DELIBERATELY absent from this list — their LOGIC (concurrent-start
# orchestration, path confinement, is_live/discovery/version-skew decisions,
# idle-shutdown predicate) is unit-tested; only the irreducible OS shell
# (fork/detach, bind, signal-wait) remains, documented per-line in
# docs/coverage.md "Residual unreachable lines".
IGNORE_REGEX='(src/main\.rs|commands/(serve|update|qmd_reindex|harness_run|search_query|search_reindex|search_model_tui)\.rs|server/chat\.rs|update/install\.rs|init/wizard\.rs|(vault_sync|output)/progress\.rs|cache/src/session_token\.rs|onebrain-search/src/embed\.rs)'

# Ratchet gate: CI fails if core line coverage drops below this. Set conservatively
# below the achieved % to absorb platform/measurement jitter; RAISE as coverage
# climbs — never lower it EXCEPT when the measured surface deliberately widens.
#
# History: 94 → 95 once the long-tail mop-up gave headroom (achieved ≈95.6% macOS /
# ≈95.5% Linux). 95 → 94 (2026-07-05) is a DELIBERATE ratchet RESET, not a
# regression: the high-assurance policy un-excluded `daemon.rs`, `daemon_client.rs`,
# and `server/search.rs` (previously whole-file-excluded), pulling their irreducible
# OS/network/embed shell into the measured surface (documented per-line under
# "Residual unreachable lines" in docs/coverage.md). Achieved after un-exclude +
# the new daemon logic tests: ≈94.99% line. 94 sits just under that. Ratchet UP from
# here as the remaining reachable gaps close.
#
# Considered and DECLINED at v3.5 Gateway PR 4 (2026-08-29, measured 94.53% line on
# macOS): 94 is the correct floor and stays. The next integer, 95, is ABOVE the
# achieved %, so it would fail CI outright; a fractional 94.5 would leave ~0.03 points
# of headroom, which is exactly the platform/measurement jitter this comment says to
# absorb. Bump this only when a run comfortably clears the next integer — that is the
# ratchet's own rule, not a reason to skip it.
CORE_LINE_THRESHOLD="${CORE_LINE_THRESHOLD:-94}"

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
