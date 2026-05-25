#!/usr/bin/env bash
# Visual UX smoke check · run before tagging a release.
#
# Builds the binary if needed, then exercises every public surface that
# matters for human + machine consumers. Capture the output and paste it
# into the PR / release notes for review.
#
# Override the binary path with ONEBRAIN_BIN=… (default: ./target/debug/onebrain).

set -euo pipefail

BIN="${ONEBRAIN_BIN:-./target/debug/onebrain}"

if [[ ! -x "$BIN" ]]; then
    echo "Building $BIN..." >&2
    cargo build --bin onebrain >&2
fi

run() {
    echo
    echo "=== $* ==="
    # || true so a non-zero exit (e.g., E_NOT_IMPLEMENTED = 72) doesn't kill
    # the whole smoke run. We're collecting visual output, not gating.
    "$BIN" "$@" 2>&1 | head -40 || true
}

# ── Banner & top-level help ────────────────────────────────────────────────
run
run --help
run --version

# ── Group-level help (every visible group must render cleanly) ─────────────
run session --help
run session init --help
run checkpoint --help
run qmd --help
run vault --help
run plugin --help

# ── Default-text behaviour (the v3.1 contract) ─────────────────────────────
# These should NOT emit JSON braces. The smoke check is "did the regression
# come back?" — review by eye.
run session init
run checkpoint orphans . tokSMOKE

# ── Structured-output modes ────────────────────────────────────────────────
run session init --json
run session init --json --pretty
run session init --yaml
run session init --output json
run session init --output yaml
run checkpoint orphans . tokSMOKE --json
run checkpoint orphans . tokSMOKE --yaml

# ── Error paths · text vs JSON ─────────────────────────────────────────────
# Run from /tmp explicitly — guaranteed no-vault.
(cd /tmp && "$BIN" session init 2>&1 | head -10 || true)
(cd /tmp && "$BIN" session init --json 2>&1 | head -10 || true)

echo
echo "DONE · paste the above into the release PR for visual review."
